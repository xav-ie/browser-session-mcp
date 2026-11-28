//! Guest grants: the agent drives tabs in a person's own Chrome.
//!
//! A takeover link brings a person's login into the operator's Chrome. A
//! guest grant goes the other way: the person (the guest) loads the extension
//! in `../extension` into their own Chrome, pastes a grant link, and the agent
//! drives one tab group there. Their cookies never leave their machine, and
//! every limit (which CDP methods, which URLs, asking before clicks) is
//! enforced by the extension, which the guest can read. This side is only a
//! relay and MCP glue; nothing here limits the agent, since from the guest's
//! side the operator, and so this server, is untrusted.
//!
//! ```text
//! MCP tool ──unix socket──▶ takeover daemon ◀──wss── extension ──chrome.debugger──▶ guest's tab
//!   (guest_tools.rs)          (Hub, below)        (outbound from the guest's Chrome)
//! ```
//!
//! - `request_guest_grant` writes a grant ticket (`guest/grants/<token>.json`)
//!   and returns the link `${TAKEOVER_BASE_URL}/guest/<token>`, and the
//!   session id `guest-<token>`.
//! - The extension fetches `.../guest/<token>/grant` to show it, and on Accept
//!   connects to `.../guest/<token>/ext`. One extension per grant; when its
//!   socket closes the grant ends for good.
//! - The MCP process reaches the daemon over `guest/relay.sock` in the shared
//!   state dir (the same file-sharing the takeover IPC relies on): a line with
//!   the token, then CDP-shaped commands, one JSON object per line. The
//!   daemon gives each its own id towards the extension and routes the reply
//!   back.
//! - The grant ends on the extension's word, its socket closing, expiry, or
//!   `revoke_guest_grant` (`guest/ended/<token>`, which the daemon polls).
use anyhow::{Context, Result, anyhow, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::frame::CloseFrame;
use async_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use async_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::takeover::{mint_token, now_ms, valid_token};

/// Session ids of guest grants start with this, then the token.
pub const SESSION_PREFIX: &str = "guest-";
pub const DEFAULT_TTL_MINUTES: u64 = 15;
/// The extension caps a grant at an hour too, whatever this server says.
pub const MAX_TTL_MINUTES: u64 = 60;
pub const MAX_PATTERNS: usize = 20;
const MAX_TEXT: usize = 200;

/// Whether the extension asks the guest before every click and Enter key.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Ask,
    Auto,
}

/// What the operator asks the guest for. Shown to the guest by the extension,
/// which enforces it; here it is only stored and served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub purpose: String,
    pub requester: String,
    pub allowed_url_patterns: Vec<String>,
    pub start_url: String,
    pub approval_mode: ApprovalMode,
    pub created_at_ms: u128,
    pub expires_at_ms: u128,
}

/// The guest session id for a token.
pub fn session_id(token: &str) -> String {
    format!("{SESSION_PREFIX}{token}")
}

/// The token of a guest session id, if it is one.
pub fn token_of(session_id: &str) -> Option<&str> {
    session_id
        .strip_prefix(SESSION_PREFIX)
        .filter(|t| valid_token(t))
}

/// A grant from the tool's arguments, checked so the operator hears about a
/// mistake now rather than from the guest. The extension checks it all again
/// (and more: the hard denylist, private addresses) before the guest sees it.
pub fn new_grant(
    purpose: &str,
    requester: &str,
    patterns: &[String],
    start_url: &str,
    ttl_minutes: Option<u64>,
    approval_mode: Option<&str>,
    now: u128,
) -> Result<Grant> {
    let text = |v: &str, name: &str| -> Result<String> {
        let v = v.trim();
        if v.is_empty() || v.chars().count() > MAX_TEXT {
            bail!("`{name}` must be 1 to {MAX_TEXT} characters");
        }
        Ok(v.to_string())
    };
    let purpose = text(purpose, "purpose")?;
    let requester = text(requester, "requester")?;
    if patterns.is_empty() || patterns.len() > MAX_PATTERNS {
        bail!("`allowedUrlPatterns` must list 1 to {MAX_PATTERNS} patterns");
    }
    let compiled = patterns
        .iter()
        .map(|p| Pattern::parse(p))
        .collect::<Result<Vec<_>>>()?;
    let start = url::Url::parse(start_url).context("`startUrl` is not a URL")?;
    if !compiled.iter().any(|p| p.matches(&start)) {
        bail!("`startUrl` matches none of `allowedUrlPatterns`");
    }
    let approval_mode = match approval_mode {
        None | Some("ask") => ApprovalMode::Ask,
        Some("auto") => ApprovalMode::Auto,
        Some(other) => bail!("unknown approvalMode `{other}` (use ask or auto)"),
    };
    let ttl = ttl_minutes
        .unwrap_or(DEFAULT_TTL_MINUTES)
        .clamp(1, MAX_TTL_MINUTES);
    Ok(Grant {
        purpose,
        requester,
        allowed_url_patterns: patterns.to_vec(),
        start_url: start_url.to_string(),
        approval_mode,
        created_at_ms: now,
        expires_at_ms: now + u128::from(ttl) * 60_000,
    })
}

/// A grant URL pattern, `scheme://host[:port]/path`: the same syntax the
/// extension's policy.js reads (see there). `*.` may start the host; `*` in
/// the path matches anything; the query and fragment are not matched.
struct Pattern {
    scheme: String,
    host: String,
    subdomains: bool,
    port: Option<u16>,
    path: regex::Regex,
}

impl Pattern {
    fn parse(p: &str) -> Result<Self> {
        let re = regex::Regex::new(r"^(https?)://([^/:]+)(?::(\d{1,5}))?(/[^?#]*)$").unwrap();
        let caps = re.captures(p).ok_or_else(|| {
            anyhow!("pattern `{p}` must be http(s)://host/path, with no query or fragment")
        })?;
        let host = caps[2].to_ascii_lowercase();
        let (subdomains, bare) = match host.strip_prefix("*.") {
            Some(rest) => (true, rest.to_string()),
            None => (false, host.clone()),
        };
        let host_ok = regex::Regex::new(r"^([a-z0-9-]+\.)*[a-z0-9-]+$").unwrap();
        if !host_ok.is_match(&bare) || (subdomains && !bare.contains('.')) {
            bail!("pattern `{p}`: the host may only start with `*.`, with no other wildcard");
        }
        let path = caps[4]
            .split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*");
        Ok(Self {
            scheme: caps[1].to_string(),
            host: bare,
            subdomains,
            port: caps.get(3).and_then(|m| m.as_str().parse().ok()),
            path: regex::Regex::new(&format!("^{path}$"))?,
        })
    }

    fn matches(&self, u: &url::Url) -> bool {
        let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
        let default_port = if self.scheme == "https" { 443 } else { 80 };
        u.scheme() == self.scheme
            && (host == self.host
                || (self.subdomains && host.ends_with(&format!(".{}", self.host))))
            && u.port_or_known_default() == Some(self.port.unwrap_or(default_port))
            && self.path.is_match(u.path())
    }
}

/// Where a grant stands, as the MCP side reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Status {
    /// Not accepted yet.
    Waiting,
    /// The guest accepted and the extension is connected.
    Active,
    /// The guest paused it; calls fail until they resume.
    Paused,
    /// Over for good.
    Ended { reason: String },
}

/// The on-disk side of guest grants, under `$STATE_DIR/takeover/guest`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store every role shares.
    pub fn shared() -> Self {
        Self::new(crate::takeover::takeover_dir().join("guest"))
    }

    fn grant_path(&self, token: &str) -> PathBuf {
        self.root.join("grants").join(format!("{token}.json"))
    }
    fn ended_path(&self, token: &str) -> PathBuf {
        self.root.join("ended").join(token)
    }
    fn status_path(&self, token: &str) -> PathBuf {
        self.root.join("status").join(token)
    }
    /// Where the daemon listens for the MCP role.
    pub fn socket_path(&self) -> PathBuf {
        self.root.join("relay.sock")
    }

    pub async fn write_grant(&self, token: &str, grant: &Grant) -> Result<()> {
        check(token)?;
        let dir = self.root.join("grants");
        tokio::fs::create_dir_all(&dir)
            .await
            .context("creating the guest grants folder")?;
        let tmp = dir.join(format!("{token}.json.tmp"));
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(grant)?)
            .await
            .context("writing the grant")?;
        tokio::fs::rename(&tmp, self.grant_path(token))
            .await
            .context("moving the grant into place")?;
        Ok(())
    }

    pub async fn read_grant(&self, token: &str) -> Result<Grant> {
        check(token)?;
        let bytes = tokio::fs::read(self.grant_path(token))
            .await
            .map_err(|_| anyhow!("unknown guest grant"))?;
        serde_json::from_slice(&bytes).context("parsing the grant")
    }

    /// End a grant. The first reason recorded stays.
    pub async fn end(&self, token: &str, reason: &str) -> Result<()> {
        check(token)?;
        let dir = self.root.join("ended");
        tokio::fs::create_dir_all(&dir).await.ok();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.ended_path(token))
            .await;
        match file {
            Ok(mut f) => f.write_all(reason.as_bytes()).await?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("ending the grant"),
        }
        let _ = tokio::fs::remove_file(self.status_path(token)).await;
        Ok(())
    }

    /// Why a grant ended, if it has.
    pub async fn ended(&self, token: &str) -> Option<String> {
        check(token).ok()?;
        tokio::fs::read_to_string(self.ended_path(token)).await.ok()
    }

    /// Record that the extension is connected (`Active`) or paused.
    async fn set_live(&self, token: &str, paused: bool) {
        let dir = self.root.join("status");
        tokio::fs::create_dir_all(&dir).await.ok();
        let _ = tokio::fs::write(
            self.status_path(token),
            if paused { "paused" } else { "active" },
        )
        .await;
    }

    /// Where a grant stands at `now`; an error for a token never issued.
    pub async fn status(&self, token: &str, now: u128) -> Result<Status> {
        let grant = self.read_grant(token).await?;
        if let Some(reason) = self.ended(token).await {
            return Ok(Status::Ended { reason });
        }
        if grant.expires_at_ms <= now {
            return Ok(Status::Ended {
                reason: "it expired".into(),
            });
        }
        Ok(
            match tokio::fs::read_to_string(self.status_path(token))
                .await
                .as_deref()
            {
                Ok("active") => Status::Active,
                Ok("paused") => Status::Paused,
                _ => Status::Waiting,
            },
        )
    }

    /// The grant, if it may still be used: issued, not ended, not expired.
    /// `Err` says why not, in words for the guest or the agent.
    pub async fn live(&self, token: &str, now: u128) -> Result<Grant, String> {
        let grant = self
            .read_grant(token)
            .await
            .map_err(|_| "unknown guest grant".to_string())?;
        if let Some(reason) = self.ended(token).await {
            return Err(format!("this grant has ended: {reason}"));
        }
        if grant.expires_at_ms <= now {
            return Err("this grant has expired".into());
        }
        Ok(grant)
    }

    /// Remove grants that ended or expired more than an hour before `now`,
    /// so their status can still be read for a while after.
    pub async fn sweep(&self, now: u128) {
        let Ok(mut entries) = tokio::fs::read_dir(self.root.join("grants")).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(token) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                let _ = tokio::fs::remove_file(entry.path()).await;
                continue;
            };
            let old = match self.read_grant(token).await {
                Ok(g) => g.expires_at_ms + 3_600_000 <= now,
                Err(_) => true,
            };
            if old {
                let _ = tokio::fs::remove_file(entry.path()).await;
                let _ = tokio::fs::remove_file(self.ended_path(token)).await;
                let _ = tokio::fs::remove_file(self.status_path(token)).await;
            }
        }
    }

    /// Every grant the daemon thought connected when it stopped: their
    /// sockets died with it, so they are over.
    async fn end_orphans(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(self.root.join("status")).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(token) = entry.file_name().to_str() {
                let _ = self.end(token, "the relay restarted").await;
            }
        }
    }
}

fn check(token: &str) -> Result<()> {
    if valid_token(token) {
        Ok(())
    } else {
        Err(anyhow!("invalid guest grant token"))
    }
}

/// Mint a token and write the grant. Returns the token.
pub async fn issue(store: &Store, grant: &Grant) -> Result<String> {
    let token = mint_token()?;
    store.write_grant(&token, grant).await?;
    Ok(token)
}

/// What to tell the agent when a grant can't take calls.
pub fn not_ready(status: &Status) -> String {
    match status {
        Status::Waiting => "The guest hasn't accepted this grant yet. Send them the grant link; they paste it into \
             the browser-session guest extension and press Accept. Calls on this session fail until then."
            .into(),
        Status::Ended { reason } => format!(
            "This guest grant has ended ({reason}). Ask for a new one with request_guest_grant if you still need it."
        ),
        Status::Active | Status::Paused => {
            "The guest's extension is not connected to the relay right now.".into()
        }
    }
}

// ---- the relay (takeover daemon) -------------------------------------------------

/// Routes commands from MCP connections to each grant's extension socket and
/// their replies back. Lives in the takeover daemon.
#[derive(Default)]
pub(crate) struct Hub {
    links: std::sync::Mutex<HashMap<String, Link>>,
    next: AtomicU64,
}

struct Link {
    /// Which connection this is, so a stale one can't detach a newer.
    conn: u64,
    to_ext: mpsc::UnboundedSender<String>,
    /// Relay id -> where its reply goes.
    pending: HashMap<u64, oneshot::Sender<Value>>,
    events: broadcast::Sender<String>,
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register the extension's socket for `token`. `None` if one is already
    /// connected: one extension per grant.
    pub(crate) fn attach(&self, token: &str) -> Option<(u64, mpsc::UnboundedReceiver<String>)> {
        let mut links = self.lock();
        if links.contains_key(token) {
            return None;
        }
        let conn = self.next.fetch_add(1, Ordering::Relaxed);
        let (to_ext, rx) = mpsc::unbounded_channel();
        links.insert(
            token.to_string(),
            Link {
                conn,
                to_ext,
                pending: HashMap::new(),
                events: broadcast::channel(32).0,
            },
        );
        Some((conn, rx))
    }

    /// The extension's socket closed: fail every call still waiting on it.
    pub(crate) fn detach(&self, token: &str, conn: u64, reason: &str) {
        let mut links = self.lock();
        if links.get(token).is_some_and(|l| l.conn == conn) {
            if let Some(link) = links.remove(token) {
                for (_, reply) in link.pending {
                    let _ = reply.send(error_reply(&format!("the guest grant ended: {reason}")));
                }
            }
        }
    }

    pub(crate) fn is_attached(&self, token: &str) -> bool {
        self.lock().contains_key(token)
    }

    /// Send a command to the extension; the receiver gets its reply (the
    /// message minus its id).
    pub(crate) fn call(
        &self,
        token: &str,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<Value>, String> {
        let mut links = self.lock();
        let link = links
            .get_mut(token)
            .ok_or_else(|| "the guest's extension is not connected".to_string())?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        link.to_ext
            .send(msg)
            .map_err(|_| "the guest's extension is not connected".to_string())?;
        link.pending.insert(id, tx);
        Ok(rx)
    }

    /// A message from the extension: a reply goes to its caller; anything
    /// else is an event, broadcast to the grant's MCP connections and
    /// returned.
    pub(crate) fn deliver(&self, token: &str, raw: &str) -> Option<Value> {
        let mut msg: Value = serde_json::from_str(raw).ok()?;
        let mut links = self.lock();
        let link = links.get_mut(token)?;
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(reply) = link.pending.remove(&id) {
                if let Some(o) = msg.as_object_mut() {
                    o.remove("id");
                }
                let _ = reply.send(msg);
            }
            return None;
        }
        msg.get("method").and_then(Value::as_str)?;
        let _ = link.events.send(msg.to_string());
        Some(msg)
    }

    pub(crate) fn subscribe(&self, token: &str) -> Option<broadcast::Receiver<String>> {
        self.lock().get(token).map(|l| l.events.subscribe())
    }
}

fn error_reply(message: &str) -> Value {
    json!({ "error": { "code": -32000, "message": message } })
}

static HUB: once_cell::sync::Lazy<Hub> = once_cell::sync::Lazy::new(Hub::default);

/// Longest WebSocket message from the extension: a full-page screenshot.
const MAX_MESSAGE: usize = 64 << 20;

/// Serve one extension socket (`GET /guest/<token>/ext` with an Upgrade)
/// until the grant ends.
pub(crate) async fn serve_extension(mut stream: TcpStream, head: &str, token: &str) -> Result<()> {
    use crate::takeover_relay::{handshake_key, reject};
    let store = Store::shared();
    let grant = match store.live(token, now_ms()).await {
        Ok(g) => g,
        Err(why) => return reject(&mut stream, "410 Gone", &why).await,
    };
    let Some(key) = handshake_key(head) else {
        return reject(&mut stream, "400 Bad Request", "not a WebSocket upgrade").await;
    };
    // Only an extension page or worker opens this; a web page can't claim it.
    if !crate::takeover::header(head, "origin")
        .is_some_and(|o| o.starts_with("chrome-extension://"))
    {
        return reject(
            &mut stream,
            "403 Forbidden",
            "only the guest extension may connect",
        )
        .await;
    }
    let Some((conn, mut outbox)) = HUB.attach(token) else {
        return reject(
            &mut stream,
            "409 Conflict",
            "an extension is already connected to this grant",
        )
        .await;
    };
    let accept = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        derive_accept_key(key.as_bytes())
    );
    if let Err(err) = stream.write_all(accept.as_bytes()).await {
        HUB.detach(token, conn, "the connection failed");
        return Err(err.into());
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let ws =
        WebSocketStream::from_raw_socket(TokioAdapter::new(stream), Role::Server, Some(config))
            .await;
    let (mut tx, mut rx) = ws.split();
    tracing::info!("guest extension connected");

    let left = u64::try_from(grant.expires_at_ms.saturating_sub(now_ms())).unwrap_or(u64::MAX);
    let expiry = tokio::time::sleep(Duration::from_millis(left));
    tokio::pin!(expiry);
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    let reason = loop {
        tokio::select! {
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let Some(ev) = HUB.deliver(token, t.as_str()) else { continue };
                    match ev.get("method").and_then(Value::as_str).unwrap_or_default() {
                        "Guest.hello" | "Guest.resumed" => store.set_live(token, false).await,
                        "Guest.paused" => store.set_live(token, true).await,
                        "Guest.ended" => {
                            let why = ev.pointer("/params/reason").and_then(Value::as_str).unwrap_or("no reason given");
                            break format!("the guest's extension ended it: {}", truncate(why, 200));
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "the guest's extension disconnected".to_string(),
                Some(Ok(_)) => {}
            },
            out = outbox.recv() => match out {
                Some(s) => {
                    if tx.send(Message::text(s)).await.is_err() {
                        break "the guest's extension disconnected".to_string();
                    }
                }
                None => break "the relay closed".to_string(),
            },
            _ = &mut expiry => break "it expired".to_string(),
            _ = poll.tick() => {
                if let Some(r) = store.ended(token).await {
                    let _ = tx.send(Message::text(json!({ "method": "Guest.revoke" }).to_string())).await;
                    break r;
                }
            },
        }
    };
    HUB.detach(token, conn, &reason);
    if let Err(err) = store.end(token, &reason).await {
        tracing::warn!(error = %err, "recording a guest grant's end failed");
    }
    tracing::info!(%reason, "guest grant ended");
    let _ = tx
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: truncate(&reason, 120).into(),
        })))
        .await;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// The grant as the extension shows it: what it asks for and how long is left.
pub(crate) async fn grant_json(token: &str) -> Result<String, (&'static str, String)> {
    let store = Store::shared();
    let grant = store
        .live(token, now_ms())
        .await
        .map_err(|why| ("410 Gone", why))?;
    if HUB.is_attached(token) {
        return Err(("409 Conflict", "this grant was already accepted".into()));
    }
    Ok(json!({
        "purpose": grant.purpose,
        "requester": grant.requester,
        "allowedUrlPatterns": grant.allowed_url_patterns,
        "startUrl": grant.start_url,
        "approvalMode": grant.approval_mode,
        "expiresInMs": grant.expires_at_ms.saturating_sub(now_ms()) as u64,
    })
    .to_string())
}

/// The guest declined: end the grant, unless their extension is connected.
pub(crate) async fn decline(token: &str) -> Result<(), &'static str> {
    if HUB.is_attached(token) {
        return Err("the grant is in use");
    }
    let store = Store::shared();
    store.read_grant(token).await.map_err(|_| "unknown grant")?;
    store
        .end(token, "the guest declined it")
        .await
        .map_err(|_| "could not record that")?;
    Ok(())
}

/// Accept MCP connections on the relay socket until the daemon stops. Also
/// ends grants a previous daemon left connected, and sweeps old ones.
pub(crate) async fn serve_socket() -> Result<()> {
    let store = Store::shared();
    tokio::fs::create_dir_all(&store.root)
        .await
        .context("creating the guest folder")?;
    store.end_orphans().await;
    let path = store.socket_path();
    let _ = tokio::fs::remove_file(&path).await;
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    tracing::info!(path = %path.display(), "guest relay socket listening");
    let sweeper = store.clone();
    tokio::spawn(async move {
        loop {
            sweeper.sweep(now_ms()).await;
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let store = store.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_mcp(stream, &store, &HUB).await {
                        tracing::debug!(error = %err, "guest relay connection error");
                    }
                });
            }
            Err(err) => tracing::warn!(error = %err, "guest relay accept failed"),
        }
    }
}

/// One MCP connection: `{"token": ...}`, answered `{"ok": true}` when the
/// grant's extension is connected (else `{"ok": false, "error": ...}`), then
/// commands `{"id", "method", "params"}`, each answered with the extension's
/// reply under the same id. Events from the extension come as lines
/// without an id.
async fn serve_mcp<S>(stream: S, store: &Store, hub: &Hub) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let first: Value = match lines.next_line().await? {
        Some(l) => serde_json::from_str(&l).unwrap_or(Value::Null),
        None => return Ok(()),
    };
    let token = first
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let refuse = match store.status(&token, now_ms()).await {
        Err(_) => Some("unknown guest grant".to_string()),
        Ok(Status::Active | Status::Paused) if hub.is_attached(&token) => None,
        Ok(s) => Some(not_ready(&s)),
    };
    if let Some(error) = refuse {
        w.write_all(format!("{}\n", json!({ "ok": false, "error": error })).as_bytes())
            .await?;
        return Ok(());
    }
    let (out, mut outbox) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = outbox.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    let _ = out.send(json!({ "ok": true }).to_string());
    let events = hub.subscribe(&token).map(|mut rx| {
        let out = out.clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let _ = out.send(ev);
            }
        })
    });
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match hub.call(&token, method, params) {
            Ok(rx) => {
                let out = out.clone();
                tokio::spawn(async move {
                    let mut reply = rx
                        .await
                        .unwrap_or_else(|_| error_reply("the guest's extension disconnected"));
                    reply["id"] = id;
                    let _ = out.send(reply.to_string());
                });
            }
            Err(why) => {
                let mut reply = error_reply(&why);
                reply["id"] = id;
                let _ = out.send(reply.to_string());
            }
        }
    }
    if let Some(e) = events {
        e.abort();
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}

// ---- the MCP side ---------------------------------------------------------------

/// A connection from the MCP role to the relay, for one tool call.
pub struct Relay {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<UnixStream>>>,
    w: tokio::io::WriteHalf<UnixStream>,
    next: u64,
    /// Events the extension sent meanwhile (a blocked navigation).
    pub events: Vec<Value>,
}

impl Relay {
    /// Connect for `token`. Fails, saying why in words for the agent, when the
    /// grant can't take calls.
    pub async fn connect(store: &Store, token: &str) -> Result<Self> {
        let status = store.status(token, now_ms()).await?;
        if !matches!(status, Status::Active | Status::Paused) {
            bail!("{}", not_ready(&status));
        }
        let path = store.socket_path();
        let stream = UnixStream::connect(&path).await.map_err(|e| {
            anyhow!(
                "can't reach the guest relay at {} ({e}); is the takeover daemon running on this host?",
                path.display()
            )
        })?;
        Self::handshake(stream, token).await
    }

    async fn handshake(stream: UnixStream, token: &str) -> Result<Self> {
        let (r, mut w) = tokio::io::split(stream);
        w.write_all(format!("{}\n", json!({ "token": token })).as_bytes())
            .await?;
        let mut lines = BufReader::new(r).lines();
        let first = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| anyhow!("the guest relay didn't answer"))??
            .ok_or_else(|| anyhow!("the guest relay closed the connection"))?;
        let v: Value = serde_json::from_str(&first)?;
        if v.get("ok") != Some(&Value::Bool(true)) {
            bail!(
                "{}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the guest relay refused the call")
            );
        }
        Ok(Self {
            lines,
            w,
            next: 1,
            events: Vec::new(),
        })
    }

    /// Send one command and wait up to `timeout` for its result. A refusal
    /// by the extension (or the guest) is an error carrying its message.
    pub async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        self.w.write_all(line.as_bytes()).await?;
        self.w.write_all(b"\n").await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .map_err(|_| {
                    anyhow!("{method}: no answer from the guest's browser within {timeout:?}")
                })??
                .ok_or_else(|| anyhow!("the guest relay closed the connection"))?;
            let v: Value = serde_json::from_str(&next)?;
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                if v.get("method").is_some() {
                    self.events.push(v);
                }
                continue;
            }
            if let Some(err) = v.get("error") {
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the guest's browser refused the command");
                bail!("{message}");
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// End a grant from the operator's side. Returns false if it had already ended.
pub async fn revoke(store: &Store, token: &str) -> Result<bool> {
    store.read_grant(token).await?;
    let already = store.ended(token).await.is_some();
    store.end(token, "the operator revoked it").await?;
    Ok(!already)
}

/// Where a store lives in tests.
#[cfg(test)]
fn test_store(name: &str) -> (Store, PathBuf) {
    let root = std::env::temp_dir().join(format!("bs-guest-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    (Store::new(root.clone()), root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    fn grant(now: u128) -> Grant {
        new_grant(
            "Install the deploy bot",
            "Ana (ops)",
            &["https://github.com/apps/*".into()],
            "https://github.com/apps/deploy-bot/installations/new",
            None,
            None,
            now,
        )
        .unwrap()
    }

    #[test]
    fn session_ids_carry_only_valid_tokens() {
        assert_eq!(token_of(&session_id(T)), Some(T));
        assert_eq!(token_of("guest-../../x"), None);
        assert_eq!(token_of("6B7F1C2D9E8A4B3C"), None);
        assert_eq!(token_of(T), None);
    }

    #[test]
    fn grants_are_checked_and_capped() {
        let g = grant(1000);
        assert_eq!(g.approval_mode, ApprovalMode::Ask);
        assert_eq!(g.expires_at_ms, 1000 + 15 * 60_000);
        let pats = ["https://github.com/apps/*".to_string()];
        let long = new_grant(
            "p",
            "r",
            &pats,
            "https://github.com/apps/x",
            Some(600),
            Some("auto"),
            0,
        )
        .unwrap();
        assert_eq!(long.expires_at_ms, 60 * 60_000, "capped at an hour");
        assert_eq!(long.approval_mode, ApprovalMode::Auto);
        let bad = |pats: &[&str], start: &str| {
            let pats: Vec<String> = pats.iter().map(|s| s.to_string()).collect();
            new_grant("p", "r", &pats, start, None, None, 0).is_err()
        };
        assert!(bad(
            &["https://github.com/apps/*"],
            "https://github.com/settings"
        ));
        assert!(bad(&["https://github.com*"], "https://github.com/"));
        assert!(bad(&["github.com/*"], "https://github.com/"));
        assert!(bad(&["https://github.com/a?b"], "https://github.com/a"));
        assert!(bad(&[], "https://github.com/"));
        assert!(!bad(
            &["https://*.example.com/*"],
            "https://a.example.com/x?y=1"
        ));
        assert!(bad(
            &["https://*.example.com/*"],
            "https://badexample.com/x"
        ));
        assert!(new_grant(" ", "r", &pats, "https://github.com/apps/x", None, None, 0).is_err());
        assert!(
            new_grant(
                "p",
                "r",
                &pats,
                "https://github.com/apps/x",
                None,
                Some("yolo"),
                0
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn tickets_expire_end_and_revoke() {
        let (store, root) = test_store("tickets");
        let g = grant(1000);
        store.write_grant(T, &g).await.unwrap();
        assert_eq!(store.read_grant(T).await.unwrap(), g);
        assert_eq!(store.status(T, 2000).await.unwrap(), Status::Waiting);
        assert!(store.live(T, 2000).await.is_ok());
        store.set_live(T, false).await;
        assert_eq!(store.status(T, 2000).await.unwrap(), Status::Active);
        store.set_live(T, true).await;
        assert_eq!(store.status(T, 2000).await.unwrap(), Status::Paused);
        // Expired: refused, whatever the status file says.
        let late = g.expires_at_ms;
        assert!(matches!(
            store.status(T, late).await.unwrap(),
            Status::Ended { .. }
        ));
        assert!(store.live(T, late).await.unwrap_err().contains("expired"));
        // Revoked: the first reason stays.
        assert!(revoke(&store, T).await.unwrap());
        assert!(!revoke(&store, T).await.unwrap(), "already ended");
        store.end(T, "something else").await.unwrap();
        assert_eq!(
            store.status(T, 2000).await.unwrap(),
            Status::Ended {
                reason: "the operator revoked it".into()
            }
        );
        assert!(store.live(T, 2000).await.unwrap_err().contains("revoked"));
        // Unknown and malformed tokens.
        assert!(store.status(&T.replace('0', "f"), 0).await.is_err());
        assert!(store.read_grant("../../etc/passwd").await.is_err());
        assert!(revoke(&store, "../x").await.is_err());
        // Swept an hour after expiry, not before.
        store.sweep(late).await;
        assert!(store.read_grant(T).await.is_ok());
        store.sweep(late + 3_600_000).await;
        assert!(store.read_grant(T).await.is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn the_hub_routes_replies_to_their_callers() {
        let hub = Hub::default();
        assert!(
            hub.call(T, "Page.navigate", json!({})).is_err(),
            "nothing attached"
        );
        let (conn, mut ext) = hub.attach(T).unwrap();
        assert!(hub.attach(T).is_none(), "one extension per grant");
        let a = hub
            .call(T, "Page.navigate", json!({"url": "https://x.test/"}))
            .unwrap();
        let b = hub.call(T, "Page.getLayoutMetrics", json!({})).unwrap();
        let first: Value = serde_json::from_str(&ext.recv().await.unwrap()).unwrap();
        let second: Value = serde_json::from_str(&ext.recv().await.unwrap()).unwrap();
        assert_eq!(first["method"], "Page.navigate");
        assert_eq!(first["params"]["url"], "https://x.test/");
        assert_ne!(first["id"], second["id"]);
        // Answered out of order: each reply still reaches its own caller.
        let reply = |id: &Value, v: Value| json!({ "id": id, "result": v }).to_string();
        assert!(
            hub.deliver(T, &reply(&second["id"], json!({"b": 2})))
                .is_none()
        );
        assert!(
            hub.deliver(T, &reply(&first["id"], json!({"a": 1})))
                .is_none()
        );
        assert_eq!(a.await.unwrap(), json!({ "result": { "a": 1 } }));
        assert_eq!(b.await.unwrap(), json!({ "result": { "b": 2 } }));
        // Events reach subscribers; unknown ids and junk are dropped.
        let mut events = hub.subscribe(T).unwrap();
        let ev = hub
            .deliver(
                T,
                r#"{"method":"Guest.navigationBlocked","params":{"origin":"https://evil.test"}}"#,
            )
            .unwrap();
        assert_eq!(ev["method"], "Guest.navigationBlocked");
        assert!(events.recv().await.unwrap().contains("evil.test"));
        assert!(hub.deliver(T, r#"{"id":999,"result":{}}"#).is_none());
        assert!(hub.deliver(T, "not json").is_none());
        // Closing fails what is still waiting, and frees the grant.
        let c = hub.call(T, "Page.captureScreenshot", json!({})).unwrap();
        hub.detach(T, conn + 1, "stale");
        assert!(
            hub.is_attached(T),
            "a stale connection can't detach a newer one"
        );
        hub.detach(T, conn, "the guest pressed End now");
        assert!(
            c.await.unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("End now")
        );
        assert!(!hub.is_attached(T));
        assert!(hub.attach(T).is_some());
    }

    #[tokio::test]
    async fn mcp_connections_reach_the_extension_through_the_hub() {
        let (store, root) = test_store("socket");
        let hub: &'static Hub = Box::leak(Box::default());
        store.write_grant(T, &grant(now_ms())).await.unwrap();

        // Not accepted yet: refused with a reason the agent can act on.
        let (client, server) = tokio::io::duplex(1 << 16);
        let s = store.clone();
        tokio::spawn(async move { serve_mcp(server, &s, hub).await });
        let (r, mut w) = tokio::io::split(client);
        let mut lines = BufReader::new(r).lines();
        w.write_all(format!("{}\n", json!({ "token": T })).as_bytes())
            .await
            .unwrap();
        let refused: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(refused["ok"], false);
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("hasn't accepted")
        );

        // Accepted and connected: commands go through, replies come back by id.
        let (_gen, mut ext) = hub.attach(T).unwrap();
        store.set_live(T, false).await;
        let (client, server) = tokio::io::duplex(1 << 16);
        let s = store.clone();
        tokio::spawn(async move { serve_mcp(server, &s, hub).await });
        let (r, mut w) = tokio::io::split(client);
        let mut lines = BufReader::new(r).lines();
        w.write_all(format!("{}\n", json!({ "token": T })).as_bytes())
            .await
            .unwrap();
        let ok: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ok["ok"], true);
        w.write_all(b"{\"id\":7,\"method\":\"Guest.tabInfo\",\"params\":{}}\n")
            .await
            .unwrap();
        let sent: Value = serde_json::from_str(&ext.recv().await.unwrap()).unwrap();
        assert_eq!(sent["method"], "Guest.tabInfo");
        let relay_id = sent["id"].clone();
        hub.deliver(
            T,
            r#"{"method":"Guest.navigationBlocked","params":{"origin":"https://x.test"}}"#,
        );
        hub.deliver(
            T,
            &json!({ "id": relay_id, "result": { "url": "https://github.com/apps/x" } })
                .to_string(),
        );
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(
                serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap()).unwrap(),
            );
        }
        let reply = got
            .iter()
            .find(|v| v["id"] == 7)
            .expect("the reply under the caller's id");
        assert_eq!(reply["result"]["url"], "https://github.com/apps/x");
        assert!(got.iter().any(|v| v["method"] == "Guest.navigationBlocked"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
