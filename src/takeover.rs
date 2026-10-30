//! Human-in-the-loop takeover.
//!
//! When the agent hits a login wall it asks a human to drive the session's
//! active page directly — so credentials never pass through the agent. The
//! flow is split across two processes that share the state dir (exactly like
//! `logs`/`saved_states` share it):
//!
//!   1. The MCP `request_human_takeover` tool mints a random token and writes a
//!      ticket file (`tokens/<token>.json`) describing which CDP target the
//!      human should drive. It returns a URL and then `await_human_takeover`
//!      blocks on a sentinel (`done/<token>`).
//!   2. This daemon (`browser-session-takeover`, host-side systemd unit) serves
//!      the takeover UI — the static Astro build under `TAKEOVER_WEBROOT` (see
//!      ../frontend). The page's JavaScript POSTs `.../claim` to receive its
//!      relay base + target, opens a WebSocket back to this daemon, runs
//!      `Page.startScreencast`, renders frames to a canvas, and forwards
//!      mouse/keyboard as `Input.dispatch*`. A "Done" button POSTs back here,
//!      dropping the sentinel and unblocking the agent.
//!
//! Chrome's DevTools endpoint has no authentication, so it stays on loopback:
//! this daemon relays the page's WebSocket to it, gated on the claim and
//! filtered to the session (see [`crate::takeover_relay`]). Otherwise it is a
//! small tokio HTTP shim: serve the built assets, mint/check claims, and
//! signal done. The UI lives in ../frontend (built separately).
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::sessions::Viewport;

/// What the human is being asked to drive. Written by the MCP tool, read by the
/// daemon when it serves the page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// CDP targetId of the page to screencast. The WS path is
    /// `/devtools/page/<targetId>`.
    #[serde(rename = "targetId")]
    pub target_id: String,
    /// Unix-millis after which the daemon refuses to serve the page.
    #[serde(rename = "expiresAtMs")]
    pub expires_at_ms: u128,
    /// The session's viewport, so the takeover UI renders every tab at the
    /// size the agent chose instead of resizing it to the human's window.
    /// Tickets minted before this field existed get the `open` default.
    #[serde(default)]
    pub viewport: Viewport,
}

/// Shared takeover dir under `STATE_DIR`. Both the container MCP and the host
/// daemon see the same path via the volume mount, so file-based IPC works
/// across the process boundary.
pub fn takeover_dir() -> PathBuf {
    crate::state_dir().join("takeover")
}

/// Public, human-facing base URL of the takeover daemon (e.g. an SSH-tunnelled
/// `http://localhost:9223` or a Traefik `https://chrome-takeover.<base>`). The
/// MCP only embeds this string in its reply; it never connects to it.
pub fn base_url() -> Option<String> {
    std::env::var("TAKEOVER_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

pub(crate) fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Unix-millis `ttl_ms` from now — for stamping a ticket's expiry.
pub fn expiry_ms(ttl_ms: u64) -> u128 {
    now_ms() + ttl_ms as u128
}

/// 32 hex chars of OS randomness. Token is the only thing guarding the page, so
/// it must be unguessable when exposed beyond loopback.
pub fn mint_token() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn tokens_dir() -> PathBuf {
    takeover_dir().join("tokens")
}
fn done_dir() -> PathBuf {
    takeover_dir().join("done")
}
fn ticket_path(token: &str) -> PathBuf {
    tokens_dir().join(format!("{token}.json"))
}
fn done_path(token: &str) -> PathBuf {
    done_dir().join(token)
}
fn claims_dir() -> PathBuf {
    takeover_dir().join("claims")
}
fn claim_path(token: &str) -> PathBuf {
    claims_dir().join(token)
}

/// Per-session "pause console capture" flag, keyed by browserContextId (==
/// ticket sessionId). When present, the listener disables the CDP `Runtime`
/// domain on that session's tabs — which stops console-log capture AND removes
/// the console-serialization side-channel that bot-detectors use to spot CDP.
/// The agent toggles it with the `set_stealth` tool; the listener polls it.
/// Path uses the default takeover dir so the listener (separate process)
/// finds it without extra config.
pub fn stealth_dir() -> PathBuf {
    takeover_dir().join("stealth")
}
/// Stealth flag path for a browserContextId. `ctx` is validated before use.
pub fn stealth_path(ctx: &str) -> PathBuf {
    stealth_dir().join(ctx)
}
/// browserContextIds are hex-ish; reject anything that could escape the dir.
fn valid_ctx(ctx: &str) -> bool {
    !ctx.is_empty() && ctx.len() <= 64 && ctx.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Set stealth for a session — the trusted MCP path. `stealth = true` writes the flag (listener
/// disables Runtime → passes CDP-based bot detection, at the cost of console
/// capture and main-world evaluate); `false` removes it.
pub async fn set_stealth(session_id: &str, stealth: bool) -> Result<()> {
    if !valid_ctx(session_id) {
        return Err(anyhow!("invalid session id"));
    }
    tokio::fs::create_dir_all(stealth_dir()).await.ok();
    let path = stealth_path(session_id);
    if stealth {
        tokio::fs::write(&path, b"1")
            .await
            .context("writing stealth flag")?;
    } else {
        let _ = tokio::fs::remove_file(&path).await;
    }
    Ok(())
}

/// Whether a human takeover is open for a session: an unexpired ticket for it
/// whose Done sentinel hasn't been dropped. Counts from the moment the ticket
/// is minted, before the human claims it, so nothing they type is ever logged.
pub async fn active(session_id: &str) -> bool {
    valid_ctx(session_id) && active_in(&tokens_dir(), &done_dir(), session_id, now_ms()).await
}

// ponytail: scans every ticket per call; fine for the handful that exist at once.
async fn active_in(tokens: &Path, done: &Path, session_id: &str, now: u128) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(tokens).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(token) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        let Ok(bytes) = tokio::fs::read(entry.path()).await else {
            continue;
        };
        let Ok(ticket) = serde_json::from_slice::<Ticket>(&bytes) else {
            continue;
        };
        if ticket.session_id == session_id
            && ticket.expires_at_ms > now
            && !tokio::fs::try_exists(done.join(token))
                .await
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Whether stealth is currently on for a session.
pub async fn is_stealth(session_id: &str) -> bool {
    if !valid_ctx(session_id) {
        return false;
    }
    tokio::fs::try_exists(stealth_path(session_id))
        .await
        .unwrap_or(false)
}

/// First-come-first-serve claim. Atomically (`create_new`) record a per-browser
/// secret the first time a token's page is served; returns:
///   Ok(Some(secret))  — caller is the claimant (send it as a cookie)
///   Ok(None)          — already claimed by `presented` (a valid reload)
///   Err(_)            — already claimed by someone else (reject)
/// so a leaked URL is useless once the real user has opened it.
async fn claim(token: &str, presented: Option<&str>) -> std::io::Result<Option<String>> {
    use std::io::{Error, ErrorKind};
    let path = claim_path(token);
    // Fast path: already claimed — only the holder of the secret may proceed.
    if let Ok(existing) = tokio::fs::read_to_string(&path).await {
        let existing = existing.trim();
        // A zero-length claim file would be matched by an empty `tk_claim=`
        // cookie — never accept it as a valid claim.
        if existing.is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "claim file corrupt",
            ));
        }
        return if presented == Some(existing) {
            Ok(None)
        } else {
            Err(Error::new(ErrorKind::PermissionDenied, "already claimed"))
        };
    }
    let secret = mint_token().map_err(|e| Error::other(e.to_string()))?;
    tokio::fs::create_dir_all(claims_dir()).await.ok();
    // Write the secret to a per-attempt temp file (named with the secret so two
    // concurrent first-claims never share a temp), then `hard_link` it into
    // place. hard_link is atomic and fails if the target exists, so the first
    // writer wins AND the claim file is never observed empty (unlike
    // create_new + a separate write).
    let tmp = claims_dir().join(format!("{token}-{secret}.tmp"));
    tokio::fs::write(&tmp, secret.as_bytes()).await?;
    let linked = tokio::fs::hard_link(&tmp, &path).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    match linked {
        Ok(()) => Ok(Some(secret)),
        // Lost the race; whoever won holds it, and it isn't us.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(Error::new(ErrorKind::PermissionDenied, "already claimed"))
        }
        Err(e) => Err(e),
    }
}

/// Check a presented cookie matches the recorded claim (for the Done POST).
/// A missing or empty claim file never matches.
async fn claim_matches(token: &str, presented: Option<&str>) -> bool {
    match tokio::fs::read_to_string(claim_path(token)).await {
        Ok(existing) => {
            let existing = existing.trim();
            !existing.is_empty() && presented == Some(existing)
        }
        Err(_) => false,
    }
}

/// Persist a ticket and return the token. Called by `request_human_takeover`.
pub async fn write_ticket(token: &str, ticket: &Ticket) -> Result<()> {
    tokio::fs::create_dir_all(tokens_dir())
        .await
        .context("creating tokens dir")?;
    let json = serde_json::to_vec_pretty(ticket).context("serializing ticket")?;
    // Write-then-rename so the daemon never reads a half-written ticket if the
    // human opens the URL within milliseconds of this call. Rename is atomic
    // within the same dir.
    let final_path = ticket_path(token);
    let tmp_path = tokens_dir().join(format!("{token}.json.tmp"));
    tokio::fs::write(&tmp_path, json)
        .await
        .context("writing ticket tmp")?;
    tokio::fs::rename(&tmp_path, &final_path)
        .await
        .context("renaming ticket into place")?;
    Ok(())
}

async fn read_ticket(token: &str) -> Result<Ticket> {
    if !valid_token(token) {
        return Err(anyhow!("invalid token"));
    }
    let bytes = tokio::fs::read(ticket_path(token))
        .await
        .context("reading ticket")?;
    serde_json::from_slice(&bytes).context("parsing ticket")
}

/// Block until the human clicks Done (sentinel appears) or the timeout elapses.
/// Returns `true` if completed, `false` on timeout. Called by
/// `await_human_takeover`. The token comes straight from the MCP caller, so it
/// is validated before it becomes a path: `../../state.json` would otherwise
/// report "completed" and then get deleted by `cleanup`.
pub async fn wait_for_done(token: &str, timeout: Duration) -> Result<bool> {
    if !valid_token(token) {
        return Err(anyhow!("invalid token"));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let path = done_path(token);
    loop {
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Best-effort cleanup of a finished/abandoned takeover's files.
pub async fn cleanup(token: &str) {
    if !valid_token(token) {
        return;
    }
    let _ = tokio::fs::remove_file(ticket_path(token)).await;
    let _ = tokio::fs::remove_file(done_path(token)).await;
    let _ = tokio::fs::remove_file(claim_path(token)).await;
}

/// Delete tickets past their expiry (with any matching done-sentinel) and stale
/// `.tmp` files. `await_human_takeover` only cleans up tickets it completes, so
/// timed-out/abandoned ones would otherwise accumulate. Best-effort.
async fn sweep_expired() {
    let Ok(mut entries) = tokio::fs::read_dir(tokens_dir()).await else {
        return;
    };
    let now = now_ms();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Leftover temp file from an interrupted write — always junk.
        if name.ends_with(".tmp") {
            let _ = tokio::fs::remove_file(&path).await;
            continue;
        }
        let Some(token) = name.strip_suffix(".json") else {
            continue;
        };
        // Unparsable ticket → treat as junk and remove; valid + expired → remove.
        let expired = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<Ticket>(&bytes)
                .map(|t| t.expires_at_ms <= now)
                .unwrap_or(true),
            Err(_) => continue,
        };
        if expired {
            let token = token.to_string();
            let _ = tokio::fs::remove_file(&path).await;
            let _ = tokio::fs::remove_file(done_path(&token)).await;
            let _ = tokio::fs::remove_file(claim_path(&token)).await;
        }
    }
}

// ---- daemon ---------------------------------------------------------------

/// Run the takeover HTTP daemon until killed. Env:
///   TAKEOVER_BIND     (default 127.0.0.1:9223) — where to listen
///   STATE_DIR         (required) — takeover IPC lives in $STATE_DIR/takeover
///   BROWSER_URL       (default http://127.0.0.1:9222) — Chrome's DevTools
///                     endpoint, which the relay connects to
pub async fn run() -> Result<()> {
    crate::init_tracing("browser_session_mcp=info");
    let bind = std::env::var("TAKEOVER_BIND").unwrap_or_else(|_| "127.0.0.1:9223".to_string());
    let browser_url = std::env::var("BROWSER_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:9222".to_string());

    tokio::fs::create_dir_all(tokens_dir()).await.ok();
    tokio::fs::create_dir_all(done_dir()).await.ok();
    tokio::fs::create_dir_all(claims_dir()).await.ok();

    let listener = TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, "browser-session-takeover listening");

    // Periodically prune expired/abandoned tickets so the state dir stays bounded.
    tokio::spawn(async {
        loop {
            sweep_expired().await;
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                continue;
            }
        };
        let browser_url = browser_url.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, &browser_url).await {
                tracing::debug!(error = %err, %peer, "connection error");
            }
        });
    }
}

/// Minimal HTTP/1.1: read the request line + headers, route, respond with
/// `Connection: close`. We serve at most a small HTML page and a 200, so a
/// single-shot handler is plenty — no keep-alive, no body streaming.
async fn handle_conn(mut stream: TcpStream, browser_url: &str) -> Result<()> {
    const MAX_HEADER: usize = 16 * 1024;
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    // Read until end-of-headers (CRLFCRLF). Requests here are tiny (no body we
    // read). Only scan the freshly-appended bytes (+3 overlap so a terminator
    // split across reads is still caught) rather than rescanning the whole
    // buffer each time.
    let mut headers_done = false;
    while buf.len() < MAX_HEADER {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break; // client closed before sending full headers
        }
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&tmp[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            headers_done = true;
            break;
        }
    }

    let r = if !headers_done {
        // Never saw the header terminator (truncated or oversized) — refuse
        // rather than parse a partial request.
        Resp::new("400 Bad Request", "text/plain", "bad request")
    } else {
        // Parse ONLY the request line (first line), not the whole buffer, so a
        // later header line can never be mistaken for the method/path.
        let head = String::from_utf8_lossy(&buf);
        let request_line = head.lines().next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("/");
        let cookie = extract_cookie(&head, "tk_claim");
        // GET /takeover/<token>/devtools/page/<targetId> with an Upgrade is
        // the page's CDP socket: hand the connection to the relay.
        if method == "GET" {
            if let Some((token, target)) = relay_path(path) {
                let head = head.into_owned();
                return crate::takeover_relay::serve(
                    stream,
                    &head,
                    &token,
                    &target,
                    cookie,
                    browser_url,
                )
                .await;
            }
        }
        route(method, path, cookie.as_deref()).await
    };
    // Mark the claim cookie Secure whenever the browser reached us over a
    // secure origin: https via the reverse proxy, or loopback (an SSH tunnel to
    // http://localhost, which browsers treat as secure). Plain http to a LAN
    // address would silently drop a Secure cookie and break Done, so that case
    // keeps working without it.
    let secure = cookie_secure(&String::from_utf8_lossy(&buf));
    let set_cookie = match &r.set_cookie {
        Some(c) if secure => format!("Set-Cookie: {c}; Secure\r\n"),
        Some(c) => format!("Set-Cookie: {c}\r\n"),
        None => String::new(),
    };
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{set_cookie}Connection: close\r\nCache-Control: no-store\r\n\r\n",
        r.status,
        r.ctype,
        r.body.len(),
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(r.body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// One HTTP response: status line + content + an optional `Set-Cookie`.
struct Resp {
    status: &'static str,
    ctype: &'static str,
    body: String,
    set_cookie: Option<String>,
}

impl Resp {
    fn new(status: &'static str, ctype: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            ctype,
            body: body.into(),
            set_cookie: None,
        }
    }
    fn cookie(mut self, c: String) -> Self {
        self.set_cookie = Some(c);
        self
    }
}

/// `(token, targetId)` from a relay path, `/takeover/<token>/devtools/page/<id>`.
fn relay_path(path: &str) -> Option<(String, String)> {
    let rest = path.split('?').next()?.strip_prefix("/takeover/")?;
    let (token, target) = rest.split_once("/devtools/page/")?;
    (valid_token(token) && !target.is_empty() && target.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| (token.to_string(), target.to_string()))
}

/// Value of the first header called `name` (case-insensitive).
pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn cookie_secure(head: &str) -> bool {
    if header(head, "x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https")) {
        return true;
    }
    let host = header(head, "host").unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Pull a single cookie value out of the request headers (case-insensitive
/// header name, first match). Returns the raw value of `name=...`.
fn extract_cookie(head: &str, name: &str) -> Option<String> {
    let line = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))?;
    let value = line.split_once(':')?.1;
    let prefix = format!("{name}=");
    value
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(&prefix))
        .map(str::to_string)
}

async fn route(method: &str, path: &str, cookie: Option<&str>) -> Resp {
    // Strip a query string if any.
    let path = path.split('?').next().unwrap_or(path);

    if method == "GET" && path == "/healthz" {
        return Resp::new("200 OK", "text/plain", "ok");
    }

    if method == "POST" {
        if let Some(rest) = path.strip_prefix("/takeover/") {
            // POST /takeover/<token>/claim — the real browser claims here, on
            // page load. Unfurl bots / prefetchers issue GETs and don't run JS,
            // so they never reach this and never learn the targetId.
            if let Some(token) = rest.strip_suffix("/claim") {
                return claim_response(token, cookie).await;
            }
            // POST /takeover/<token>/done — only the claimant (matching cookie).
            if let Some(token) = rest.strip_suffix("/done") {
                if !valid_token(token) {
                    return Resp::new("400 Bad Request", "text/plain", "bad token");
                }
                if !claim_matches(token, cookie).await {
                    return Resp::new("403 Forbidden", "text/plain", "not the claimant");
                }
                if let Err(err) = mark_done(token).await {
                    tracing::warn!(error = %err, "marking done failed");
                    return Resp::new("500 Internal Server Error", "text/plain", "error");
                }
                return Resp::new("200 OK", "text/plain", "done");
            }
        }
    }

    // GET: serve the static Astro app. Any /takeover/<token> gets the same
    // index.html shell — validity is enforced by POST .../claim, and the token
    // lives in the URL (client reads it from location.pathname). Everything else
    // is a built asset (/_astro/*, favicon, …).
    if method == "GET" {
        if path.starts_with("/takeover/") {
            return serve_file("index.html").await;
        }
        return serve_file(path.trim_start_matches('/')).await;
    }

    Resp::new("404 Not Found", "text/plain", "not found")
}

/// Directory the Astro build's `dist` was installed to (set by the package's
/// wrapper, or the systemd unit). The daemon serves the takeover UI from here.
fn webroot() -> Option<PathBuf> {
    std::env::var("TAKEOVER_WEBROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// Serve a built static file from TAKEOVER_WEBROOT. Path-traversal-safe (rejects
/// empty paths and any `..` component). The Astro build emits only text assets
/// (HTML/JS, CSS inlined), so reading as UTF-8 is fine.
async fn serve_file(rel: &str) -> Resp {
    let Some(root) = webroot() else {
        return Resp::new(
            "500 Internal Server Error",
            "text/plain",
            "takeover webroot not configured (set TAKEOVER_WEBROOT)",
        );
    };
    if rel.is_empty() || rel.split('/').any(|c| c == "..") {
        return Resp::new("404 Not Found", "text/plain", "not found");
    }
    match tokio::fs::read_to_string(root.join(rel)).await {
        Ok(body) => Resp::new("200 OK", content_type(rel), body),
        Err(_) => Resp::new("404 Not Found", "text/plain", "not found"),
    }
}

/// First-come-first-serve claim, triggered by the page's on-load POST. Returns
/// the target and the relay base (a path on this daemon) only to the winning
/// claimant or a matching reload; a different client gets 409.
async fn claim_response(token: &str, cookie: Option<&str>) -> Resp {
    if !valid_token(token) {
        return Resp::new("400 Bad Request", "text/plain", "bad token");
    }
    match read_ticket(token).await {
        Ok(t) if t.expires_at_ms > now_ms() => match claim(token, cookie).await {
            Ok(maybe_secret) => {
                // Hand back the relay base + this session's context id so the page
                // can enumerate the session's tabs (Target.getTargets filtered by
                // browserContextId) and switch between them, plus the session
                // viewport every tab is rendered at.
                let body = format!(
                    "{{\"wsBase\":{},\"targetId\":{},\"ctxId\":{},\"viewport\":{{\"width\":{},\"height\":{}}}}}",
                    js_str(&format!("/takeover/{token}")),
                    js_str(&t.target_id),
                    js_str(&t.session_id),
                    t.viewport.width,
                    t.viewport.height,
                );
                let resp = Resp::new("200 OK", "application/json", body);
                // New claimant → hand them the claim cookie (scoped to this
                // token's path). A reload (Ok(None)) already has it.
                match maybe_secret {
                    Some(secret) => resp.cookie(format!(
                        "tk_claim={secret}; Path=/takeover/{token}; HttpOnly; SameSite=Strict"
                    )),
                    None => resp,
                }
            }
            Err(_) => Resp::new(
                "409 Conflict",
                "text/plain",
                "this takeover link is already in use by someone else",
            ),
        },
        Ok(_) => Resp::new("410 Gone", "text/plain", "this takeover link has expired"),
        Err(_) => Resp::new(
            "404 Not Found",
            "text/plain",
            "unknown or consumed takeover token",
        ),
    }
}

async fn mark_done(token: &str) -> Result<()> {
    tokio::fs::create_dir_all(done_dir()).await.ok();
    tokio::fs::write(done_path(token), b"done").await?;
    // Close this token's open relay sockets; nobody listening is fine.
    let _ = DONE.send(token.to_string());
    Ok(())
}

/// Tokens marked done, broadcast to open relays (see [`done_events`]).
static DONE: once_cell::sync::Lazy<tokio::sync::broadcast::Sender<String>> =
    once_cell::sync::Lazy::new(|| tokio::sync::broadcast::channel(64).0);

/// Subscribe to Done clicks, so a relay closes the moment its takeover ends.
pub(crate) fn done_events() -> tokio::sync::broadcast::Receiver<String> {
    DONE.subscribe()
}

/// Whether a relay socket for `token` may run: the ticket exists and hasn't
/// expired, Done hasn't been clicked, and `presented` is the claim cookie.
/// Returns the session's browserContextId and the ticket's expiry.
pub(crate) async fn relay_gate(token: &str, presented: Option<&str>) -> Option<(String, u128)> {
    let ticket = read_ticket(token).await.ok()?;
    if ticket.expires_at_ms <= now_ms() {
        return None;
    }
    if tokio::fs::try_exists(done_path(token))
        .await
        .unwrap_or(true)
    {
        return None;
    }
    if !claim_matches(token, presented).await {
        return None;
    }
    Some((ticket.session_id, ticket.expires_at_ms))
}

/// Tokens are 32 lowercase hex chars (see `mint_token`). Reject anything else
/// so a crafted path can't escape the tokens dir or match a sentinel.
fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// JSON-encode a string for safe embedding in a JSON response.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        Ticket, Viewport, active_in, cleanup, cookie_secure, is_stealth, read_ticket, valid_ctx,
        valid_token, wait_for_done,
    };
    use std::time::Duration;

    const TRAVERSALS: [&str; 5] = [
        "../../state.json",
        "../../../../etc/passwd",
        "/etc/passwd",
        "0123456789abcdef0123456789abcde/",
        "",
    ];

    #[test]
    fn rejects_traversal_ids() {
        assert!(valid_token("0123456789abcdef0123456789abcdef"));
        assert!(valid_ctx("6B7F1C2D9E8A4B3C"));
        for bad in TRAVERSALS {
            assert!(!valid_token(bad), "{bad}");
            assert!(!valid_ctx(bad), "{bad}");
        }
    }

    // These run before STATE_DIR is initialised: any of them building a path
    // from the bad id would panic in `state_dir()`, so passing proves the id
    // is refused first.
    #[tokio::test]
    async fn caller_supplied_ids_never_become_paths() {
        for bad in TRAVERSALS {
            assert!(wait_for_done(bad, Duration::ZERO).await.is_err(), "{bad}");
            assert!(read_ticket(bad).await.is_err(), "{bad}");
            assert!(!is_stealth(bad).await, "{bad}");
            cleanup(bad).await;
        }
    }

    #[tokio::test]
    async fn takeover_active_until_done_or_expired() {
        let root = std::env::temp_dir().join(format!("bs-takeover-{}", std::process::id()));
        let (tokens, done) = (root.join("tokens"), root.join("done"));
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::create_dir_all(&done).unwrap();
        let ticket = |session: &str, expires_at_ms| Ticket {
            session_id: session.into(),
            target_id: "T".into(),
            expires_at_ms,
            viewport: Viewport::default(),
        };
        let write = |token: &str, t: &Ticket| {
            std::fs::write(
                tokens.join(format!("{token}.json")),
                serde_json::to_vec(t).unwrap(),
            )
            .unwrap()
        };
        assert!(!active_in(&tokens, &done, "CTX1", 1000).await);
        write("aa", &ticket("CTX1", 2000));
        write("bb", &ticket("CTX2", 500));
        assert!(active_in(&tokens, &done, "CTX1", 1000).await);
        assert!(!active_in(&tokens, &done, "CTX2", 1000).await, "expired");
        assert!(
            !active_in(&tokens, &done, "CTX3", 1000).await,
            "other session"
        );
        std::fs::write(done.join("aa"), b"").unwrap();
        assert!(!active_in(&tokens, &done, "CTX1", 1000).await, "done");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn secure_cookie_origins() {
        let req = |h: &str| format!("POST /takeover/t/claim HTTP/1.1\r\n{h}\r\n\r\n");
        assert!(cookie_secure(&req(
            "Host: chrome-takeover.example.com\r\nX-Forwarded-Proto: https"
        )));
        assert!(cookie_secure(&req("Host: localhost:9223")));
        assert!(cookie_secure(&req("host: 127.0.0.1:9223")));
        assert!(cookie_secure(&req("Host: [::1]:9223")));
        assert!(!cookie_secure(&req("Host: 192.168.1.5:9223")));
        assert!(!cookie_secure(&req(
            "Host: chrome-takeover.example.com\r\nX-Forwarded-Proto: http"
        )));
    }
}
