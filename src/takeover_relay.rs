//! Token-gated CDP relay for the takeover page.
//!
//! The takeover page drives one tab at a time over a page-level DevTools
//! WebSocket. Chrome's DevTools endpoint has no authentication of its own, so
//! instead of exposing it, the takeover daemon accepts that WebSocket at
//! `/takeover/<token>/devtools/page/<targetId>` and relays it to the Chrome on
//! loopback:
//!
//! ```text
//! takeover page ──wss──▶ daemon ──ws (loopback)──▶ Chrome
//!                 gate: live ticket + claim cookie + target in the session
//!                 filter: allowlisted methods, session-scoped Target.*
//! ```
//!
//! The socket ends when the human clicks Done (the same daemon handles that
//! POST and signals open relays directly) or when the link expires (a timer
//! at the ticket's expiry), so nothing polls. The filter is what keeps a link holder inside their own
//! session: a page-level socket could otherwise list, attach to or close any
//! tab in Chrome, and navigate to `file://` or `chrome://` URLs.
use anyhow::{Context, Result, anyhow, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::Role;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use url::Url;

use crate::chrome;

/// CDP methods the takeover page uses (frontend/src/lib/takeover.ts). Target
/// methods are further checked or rewritten in [`Filter::inbound`].
const ALLOWED: &[&str] = &[
    "Emulation.setDeviceMetricsOverride",
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.bringToFront",
    "Page.enable",
    "Page.getNavigationHistory",
    "Page.navigate",
    "Page.screencastFrameAck",
    "Page.startScreencast",
    "Page.stopScreencast",
    "Runtime.evaluate",
    "Target.activateTarget",
    "Target.closeTarget",
    "Target.createTarget",
    "Target.getTargets",
    "Target.setDiscoverTargets",
];

/// Relay one upgraded takeover WebSocket until either side closes or the gate
/// stops passing. `head` is the raw request head; the gate already passed once.
pub(crate) async fn serve(
    mut stream: TcpStream,
    head: &str,
    token: &str,
    target_id: &str,
    cookie: Option<String>,
    browser_url: &str,
) -> Result<()> {
    // Subscribe before checking the gate, so a Done landing in between is
    // still seen below.
    let mut done = crate::takeover::done_events();
    let (ctx, expires_at_ms) = match crate::takeover::relay_gate(token, cookie.as_deref()).await {
        Some(v) => v,
        None => return reject(&mut stream, "403 Forbidden", "takeover link is not active").await,
    };
    let Some(key) = handshake_key(head) else {
        return reject(&mut stream, "400 Bad Request", "not a WebSocket upgrade").await;
    };
    if !same_origin(head) {
        return reject(&mut stream, "403 Forbidden", "cross-origin WebSocket").await;
    }
    let (page_url, known) = match session_targets(browser_url, &ctx, target_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "takeover relay: target lookup failed");
            return reject(&mut stream, "502 Bad Gateway", "could not reach Chrome").await;
        }
    };
    if !known.contains(target_id) {
        return reject(&mut stream, "403 Forbidden", "tab is not in this session").await;
    }
    let (chrome_ws, _) = async_tungstenite::tokio::connect_async(page_url.as_str())
        .await
        .context("connecting to the tab's DevTools socket")?;

    stream
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\n\r\n",
                derive_accept_key(key.as_bytes())
            )
            .as_bytes(),
        )
        .await?;
    let client =
        WebSocketStream::from_raw_socket(TokioAdapter::new(stream), Role::Server, None).await;

    let (mut c_tx, mut c_rx) = client.split();
    let (mut b_tx, mut b_rx) = chrome_ws.split();
    let mut filter = Filter::new(ctx, known);
    let left = Duration::from_millis(
        u64::try_from(expires_at_ms.saturating_sub(crate::takeover::now_ms())).unwrap_or(u64::MAX),
    );
    let expiry = tokio::time::sleep(left);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            m = c_rx.next() => match m {
                Some(Ok(Message::Text(t))) => match filter.inbound(t.as_str()) {
                    Ok(fwd) => b_tx.send(Message::text(fwd)).await?,
                    Err(reply) => c_tx.send(Message::text(reply)).await?,
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                // CDP is JSON text; pings are answered by tungstenite itself.
                Some(Ok(_)) => {}
            },
            m = b_rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    if let Some(out) = filter.outbound(t.as_str()) {
                        c_tx.send(Message::text(out)).await?;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            d = done.recv() => match d {
                Ok(t) if t == token => break,
                Ok(_) => {}
                // Missed notifications: re-check this token once.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if crate::takeover::relay_gate(token, cookie.as_deref()).await.is_none() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = &mut expiry => break,
        }
    }
    let _ = c_tx.send(Message::Close(None)).await;
    let _ = b_tx.close(None).await;
    Ok(())
}

async fn reject(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// The client's `Sec-WebSocket-Key`, if this is a version-13 upgrade.
fn handshake_key(head: &str) -> Option<String> {
    let upgrade = crate::takeover::header(head, "upgrade")?;
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return None;
    }
    if crate::takeover::header(head, "sec-websocket-version") != Some("13") {
        return None;
    }
    crate::takeover::header(head, "sec-websocket-key").map(str::to_string)
}

/// Browsers always send `Origin` on a WebSocket upgrade; require it to name
/// the host the page was served from. The claim cookie is `SameSite=Strict`
/// already, so this is a second fence, not the only one.
fn same_origin(head: &str) -> bool {
    let Some(host) = crate::takeover::header(head, "host") else {
        return false;
    };
    let Some(origin) = crate::takeover::header(head, "origin") else {
        return false;
    };
    Url::parse(origin).is_ok_and(|o| {
        let authority = match (o.host_str(), o.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => return false,
        };
        authority.eq_ignore_ascii_case(host)
    })
}

/// Ask Chrome (browser-level socket) for the session's targets. Returns the
/// requested tab's DevTools URL and every target id in the session.
async fn session_targets(
    browser_url: &str,
    ctx: &str,
    target_id: &str,
) -> Result<(Url, HashSet<String>)> {
    let browser_ws = chrome::resolve_ws_endpoint(browser_url).await?;
    let (mut ws, _) = async_tungstenite::tokio::connect_async(browser_ws.as_str())
        .await
        .context("connecting to Chrome")?;
    ws.send(Message::text(
        json!({ "id": 1, "method": "Target.getTargets" }).to_string(),
    ))
    .await?;
    let infos = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .map_err(|_| anyhow!("Target.getTargets timed out"))?
            .ok_or_else(|| anyhow!("Chrome closed the socket"))??;
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(t.as_str())?;
        if v.get("id").and_then(Value::as_u64) == Some(1) {
            break v
                .pointer("/result/targetInfos")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| anyhow!("Target.getTargets returned no targetInfos"))?;
        }
    };
    let _ = ws.close(None).await;
    let known = infos
        .iter()
        .filter(|t| t.get("browserContextId").and_then(Value::as_str) == Some(ctx))
        .filter_map(|t| {
            t.get("targetId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if !target_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        bail!("malformed target id");
    }
    let mut page = Url::parse(&browser_ws)?;
    page.set_path(&format!("/devtools/page/{target_id}"));
    page.set_query(None);
    Ok((page, known))
}

/// Navigation targets a takeover may load: the web, and a blank tab. Not
/// `file:` (local files on the Chrome host), `chrome:` (browser settings,
/// saved passwords) or `javascript:`.
fn allowed_url(u: &str) -> bool {
    u == "about:blank" || Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Per-connection message filter. Keeps the link holder inside their session:
/// only allowlisted methods pass, Target calls must name a session target (or
/// are pinned to the session's context), and Target results and events from
/// other sessions are dropped before they reach the page.
pub(crate) struct Filter {
    ctx: String,
    /// Target ids known to belong to the session; grows from target events,
    /// getTargets results and createTarget replies.
    known: HashSet<String>,
    /// Request ids whose replies need filtering or recording.
    get_targets: HashSet<u64>,
    create_target: HashSet<u64>,
}

impl Filter {
    pub(crate) fn new(ctx: String, known: HashSet<String>) -> Self {
        Self {
            ctx,
            known,
            get_targets: HashSet::new(),
            create_target: HashSet::new(),
        }
    }

    /// Check (and maybe rewrite) a page → Chrome message. `Err` carries the
    /// CDP error reply to send back to the page instead.
    pub(crate) fn inbound(&mut self, raw: &str) -> Result<String, String> {
        let Ok(mut msg) = serde_json::from_str::<Value>(raw) else {
            return Err(deny(&Value::Null, "unparsable message"));
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        // Flattened child sessions would route around every check below.
        if msg.get("sessionId").is_some() {
            return Err(deny(&id, "sessionId routing is not allowed"));
        }
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !ALLOWED.contains(&method.as_str()) {
            return Err(deny(&id, &format!("{method} is not allowed")));
        }
        let param = |k: &str| {
            msg.pointer(&format!("/params/{k}"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match method.as_str() {
            "Target.getTargets" => {
                if let Some(n) = id.as_u64() {
                    self.get_targets.insert(n);
                }
            }
            "Target.activateTarget" | "Target.closeTarget"
                if !param("targetId").is_some_and(|t| self.known.contains(&t)) =>
            {
                return Err(deny(&id, "that tab is not in this session"));
            }
            "Target.createTarget" => {
                if !param("url").is_some_and(|u| allowed_url(&u)) {
                    return Err(deny(&id, "only http(s) and about:blank tabs can be opened"));
                }
                if !msg.get("params").is_some_and(Value::is_object) {
                    msg["params"] = json!({});
                }
                msg["params"]["browserContextId"] = json!(self.ctx);
                if let Some(n) = id.as_u64() {
                    self.create_target.insert(n);
                }
            }
            "Page.navigate" if !param("url").is_some_and(|u| allowed_url(&u)) => {
                return Err(deny(&id, "only http(s) and about:blank can be loaded"));
            }
            _ => {}
        }
        Ok(msg.to_string())
    }

    /// Filter a Chrome → page message; `None` drops it.
    pub(crate) fn outbound(&mut self, raw: &str) -> Option<String> {
        let mut msg: Value = serde_json::from_str(raw).ok()?;
        if let Some(n) = msg.get("id").and_then(Value::as_u64) {
            if self.get_targets.remove(&n) {
                if let Some(infos) = msg
                    .pointer_mut("/result/targetInfos")
                    .and_then(Value::as_array_mut)
                {
                    infos.retain(|t| self.in_session(t));
                    for t in infos.iter() {
                        if let Some(tid) = t.get("targetId").and_then(Value::as_str) {
                            self.known.insert(tid.to_string());
                        }
                    }
                }
                return Some(msg.to_string());
            }
            if self.create_target.remove(&n) {
                if let Some(tid) = msg.pointer("/result/targetId").and_then(Value::as_str) {
                    self.known.insert(tid.to_string());
                }
            }
            return Some(raw.to_string());
        }
        match msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let info = msg.pointer("/params/targetInfo")?;
                if !self.in_session(info) {
                    return None;
                }
                let tid = info.get("targetId").and_then(Value::as_str)?.to_string();
                self.known.insert(tid);
            }
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                let tid = msg.pointer("/params/targetId").and_then(Value::as_str)?;
                if !self.known.remove(tid) {
                    return None;
                }
            }
            // Attach is never allowed, so these could only describe others.
            "Target.attachedToTarget"
            | "Target.detachedFromTarget"
            | "Target.receivedMessageFromTarget" => return None,
            _ => {}
        }
        Some(raw.to_string())
    }

    fn in_session(&self, info: &Value) -> bool {
        info.get("browserContextId").and_then(Value::as_str) == Some(self.ctx.as_str())
    }
}

fn deny(id: &Value, why: &str) -> String {
    json!({
        "id": id,
        "error": { "code": -32601, "message": format!("takeover relay: {why}") }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> Filter {
        Filter::new("CTX".into(), ["T1".to_string()].into_iter().collect())
    }

    #[test]
    fn only_allowlisted_methods_pass() {
        let mut f = filter();
        assert!(f.inbound(r#"{"id":1,"method":"Page.enable"}"#).is_ok());
        for bad in [
            r#"{"id":2,"method":"Target.attachToTarget","params":{"targetId":"T1"}}"#,
            r#"{"id":3,"method":"Storage.getCookies"}"#,
            r#"{"id":4,"method":"Network.getAllCookies"}"#,
            r#"{"id":5,"method":"Page.enable","sessionId":"S"}"#,
            "not json",
        ] {
            let err = f.inbound(bad).unwrap_err();
            assert!(err.contains("takeover relay"), "{bad} -> {err}");
        }
    }

    #[test]
    fn target_calls_stay_in_the_session() {
        let mut f = filter();
        assert!(
            f.inbound(r#"{"id":1,"method":"Target.activateTarget","params":{"targetId":"T1"}}"#)
                .is_ok()
        );
        assert!(
            f.inbound(r#"{"id":2,"method":"Target.closeTarget","params":{"targetId":"OTHER"}}"#)
                .is_err()
        );
        let created = f
            .inbound(r#"{"id":3,"method":"Target.createTarget","params":{"url":"about:blank","browserContextId":"OTHER"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&created).unwrap();
        assert_eq!(v["params"]["browserContextId"], "CTX");
        // The new tab becomes known from the reply, so it can be closed.
        f.outbound(r#"{"id":3,"result":{"targetId":"T2"}}"#)
            .unwrap();
        assert!(
            f.inbound(r#"{"id":4,"method":"Target.closeTarget","params":{"targetId":"T2"}}"#)
                .is_ok()
        );
    }

    #[test]
    fn navigation_is_limited_to_the_web() {
        let mut f = filter();
        for ok in ["https://example.com", "http://x.test/a", "about:blank"] {
            let m = json!({"id":1,"method":"Page.navigate","params":{"url":ok}}).to_string();
            assert!(f.inbound(&m).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "chrome://settings",
            "javascript:alert(1)",
        ] {
            let m = json!({"id":1,"method":"Page.navigate","params":{"url":bad}}).to_string();
            assert!(f.inbound(&m).is_err(), "{bad}");
            let m = json!({"id":1,"method":"Target.createTarget","params":{"url":bad}}).to_string();
            assert!(f.inbound(&m).is_err(), "{bad}");
        }
    }

    #[test]
    fn other_sessions_are_hidden_from_the_page() {
        let mut f = filter();
        f.inbound(r#"{"id":7,"method":"Target.getTargets"}"#)
            .unwrap();
        let out = f
            .outbound(
                r#"{"id":7,"result":{"targetInfos":[
                {"targetId":"T1","browserContextId":"CTX","type":"page"},
                {"targetId":"X1","browserContextId":"OTHER","type":"page"}]}}"#,
            )
            .unwrap();
        assert!(out.contains("T1") && !out.contains("X1"));
        assert!(
            f.outbound(r#"{"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"X2","browserContextId":"OTHER"}}}"#)
                .is_none()
        );
        assert!(
            f.outbound(r#"{"method":"Target.targetDestroyed","params":{"targetId":"X2"}}"#)
                .is_none()
        );
        assert!(
            f.outbound(r#"{"method":"Target.targetDestroyed","params":{"targetId":"T1"}}"#)
                .is_some()
        );
        // Ordinary page traffic passes untouched.
        let frame = r#"{"method":"Page.screencastFrame","params":{"sessionId":1}}"#;
        assert_eq!(f.outbound(frame).as_deref(), Some(frame));
    }

    #[test]
    fn origin_must_match_host() {
        let head = |origin: &str| {
            format!("GET / HTTP/1.1\r\nHost: takeover.example.com\r\nOrigin: {origin}\r\n\r\n")
        };
        assert!(same_origin(&head("https://takeover.example.com")));
        assert!(!same_origin(&head("https://evil.example.com")));
        assert!(!same_origin(
            "GET / HTTP/1.1\r\nHost: takeover.example.com\r\n\r\n"
        ));
        assert!(same_origin(
            "GET / HTTP/1.1\r\nHost: localhost:9223\r\nOrigin: http://localhost:9223\r\n\r\n"
        ));
    }
}
