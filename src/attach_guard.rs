//! Mask new tabs before their first request.
//!
//! The listener's per-tab masking (UA override) attaches to a
//! tab only after Chrome has created it, and a tab a page opens itself
//! (`window.open`, `target=_blank`) is already loading by then: its first
//! request goes out as `HeadlessChrome` while its JS later reports Chrome.
//!
//! This guard holds a raw CDP connection with browser-wide auto-attach and
//! `waitForDebuggerOnStart`, so every new page target is created paused. While
//! paused it gets its session's UA override, then it is
//! resumed. chromiumoxide can't do this: it resumes paused targets itself with
//! no hook in between. The guard stays attached for each tab's lifetime,
//! because overrides end with the CDP session that set them.
//!
//! One request escapes that: a `target=_blank` (noopener) link's first
//! navigation has its `Sec-CH-UA` client hints computed by Chrome before the
//! tab exists for DevTools, so no override can reach them, even though the
//! request itself is only sent after the tab is resumed. For that, the guard
//! intercepts each new tab's first document request (`Fetch`) and rewrites the
//! client hints if they still say `HeadlessChrome`, then stops intercepting.
//!
//! A tab must never stay frozen, so every attached target is resumed even when
//! masking it fails, and if this connection drops Chrome releases the targets
//! it was holding.
//!
//! The same per-tab sessions carry each session's throttling and init scripts
//! (see `emulation`): applied while a new tab is paused, and re-synced from the
//! session record on a short tick for tabs already open.
use anyhow::{Context, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

use crate::chrome;
use crate::emulation::{self, Cmd, TabApplied};
use crate::state::{SessionRecord, read_session, sessions_dir};
use crate::user_agent::UaOverride;

const SYNC_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Cdp {
    next_id: u64,
}

impl Cdp {
    fn msg(&mut self, method: &str, params: Value, session: Option<&str>) -> (u64, Message) {
        self.next_id += 1;
        let mut msg = json!({ "id": self.next_id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        (self.next_id, Message::text(msg.to_string()))
    }
}

struct Tab {
    ctx: String,
    applied: TabApplied,
}

/// A command whose reply the guard waits for.
struct Pending {
    ctx: String,
    /// The tab session and init script id, for `addScriptToEvaluateOnNewDocument`.
    script: Option<(String, String)>,
}

/// Emulation bookkeeping across all tabs.
#[derive(Default)]
struct Sync {
    tabs: HashMap<String, Tab>,
    pending: HashMap<u64, Pending>,
    /// Replies still outstanding per session context.
    outstanding: HashMap<String, usize>,
    /// The record revision last planned per context, and the one last reported.
    latest: HashMap<String, u64>,
    reported: HashMap<String, u64>,
}

impl Sync {
    /// Messages that bring tab `session` up to `rec`.
    fn plan(&mut self, cdp: &mut Cdp, session: &str, rec: &SessionRecord) -> Vec<Message> {
        let Some(tab) = self.tabs.get_mut(session) else {
            return Vec::new();
        };
        let ctx = tab.ctx.clone();
        let cmds = emulation::plan(
            &rec.emulation.clone().unwrap_or_default(),
            &rec.init_scripts,
            &mut tab.applied,
        );
        self.latest.insert(ctx.clone(), rec.emulation_rev);
        let mut out = Vec::with_capacity(cmds.len());
        for cmd in cmds {
            let (id, msg, script) = match cmd {
                Cmd::Send(method, params) => {
                    let (id, msg) = cdp.msg(method, params, Some(session));
                    (id, msg, None)
                }
                Cmd::AddScript(script_id, params) => {
                    let (id, msg) = cdp.msg(
                        "Page.addScriptToEvaluateOnNewDocument",
                        params,
                        Some(session),
                    );
                    (id, msg, Some((session.to_string(), script_id)))
                }
            };
            self.pending.insert(
                id,
                Pending {
                    ctx: ctx.clone(),
                    script,
                },
            );
            *self.outstanding.entry(ctx.clone()).or_default() += 1;
            out.push(msg);
        }
        out
    }

    /// Handle a reply to a tracked command. Returns follow-up messages (a
    /// script removed while its add was in flight).
    fn reply(&mut self, cdp: &mut Cdp, id: u64, v: &Value) -> Option<(String, Vec<Message>)> {
        let p = self.pending.remove(&id)?;
        if let Some(err) = v.get("error") {
            tracing::warn!(%err, ctx = %p.ctx, "emulation command failed");
        }
        let mut follow = Vec::new();
        if let Some((session, script_id)) = p.script {
            let identifier = v["result"]["identifier"].as_str().map(str::to_string);
            if let (Some(tab), Some(identifier)) = (self.tabs.get_mut(&session), identifier) {
                match tab.applied.scripts.get_mut(&script_id) {
                    Some(slot) => *slot = Some(identifier),
                    None => {
                        let (_, msg) = cdp.msg(
                            "Page.removeScriptToEvaluateOnNewDocument",
                            json!({ "identifier": identifier }),
                            Some(&session),
                        );
                        follow.push(msg);
                    }
                }
            }
        }
        if let Some(n) = self.outstanding.get_mut(&p.ctx) {
            *n = n.saturating_sub(1);
        }
        Some((p.ctx, follow))
    }

    /// The revision to report for `ctx`, if every command for it has been
    /// answered and it hasn't been reported yet.
    fn settled(&mut self, ctx: &str) -> Option<u64> {
        if self.outstanding.get(ctx).copied().unwrap_or(0) > 0 {
            return None;
        }
        let rev = *self.latest.get(ctx)?;
        if rev == 0 || self.reported.get(ctx) == Some(&rev) {
            return None;
        }
        self.reported.insert(ctx.to_string(), rev);
        Some(rev)
    }
}

/// Run until the connection drops. `desktop` is the listener's default UA for
/// sessions without a stored one.
pub async fn run(browser_url: &str, desktop: Option<UaOverride>) -> Result<()> {
    let ws_url = chrome::resolve_ws_endpoint(browser_url).await?;
    let (mut ws, _) = async_tungstenite::tokio::connect_async(ws_url)
        .await
        .context("attach guard connect")?;
    let mut cdp = Cdp::default();

    ws.send(
        cdp.msg(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [{ "type": "page", "exclude": false }, { "exclude": true }],
            }),
            None,
        )
        .1,
    )
    .await?;
    tracing::info!("attach guard active");

    // Tabs whose first document request is still to be checked, by session.
    let mut first_request: HashMap<String, UaOverride> = HashMap::new();
    let mut sync = Sync::default();
    let mut tick = tokio::time::interval(SYNC_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let msg = tokio::select! {
            msg = ws.next() => match msg {
                Some(msg) => msg.context("attach guard read")?,
                None => break,
            },
            _ = tick.tick() => {
                let mut ctxs: Vec<String> = sync.tabs.values().map(|t| t.ctx.clone()).collect();
                ctxs.sort();
                ctxs.dedup();
                for ctx in ctxs {
                    let Some(rec) = read_session(&sessions_dir(), &ctx).await else { continue };
                    let sessions: Vec<String> = sync
                        .tabs
                        .iter()
                        .filter(|(_, t)| t.ctx == ctx)
                        .map(|(s, _)| s.clone())
                        .collect();
                    for session in sessions {
                        for m in sync.plan(&mut cdp, &session, &rec) {
                            ws.send(m).await?;
                        }
                    }
                    report(&mut sync, &ctx).await;
                }
                continue;
            }
        };
        let Ok(text) = msg.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some(id) = v["id"].as_u64() {
            if let Some((ctx, follow)) = sync.reply(&mut cdp, id, &v) {
                for m in follow {
                    ws.send(m).await?;
                }
                report(&mut sync, &ctx).await;
                continue;
            }
        }
        if let Some(err) = v.get("error") {
            tracing::warn!(%err, "attach guard command failed");
            continue;
        }
        if v["method"] == "Fetch.requestPaused" {
            let Some(session) = v["sessionId"].as_str() else {
                continue;
            };
            let p = &v["params"];
            let mut params = json!({ "requestId": p["requestId"] });
            if let Some(ua) = first_request.remove(session) {
                if let Some(headers) = fixed_client_hints(&p["request"]["headers"], &ua) {
                    params["headers"] = headers;
                }
            }
            ws.send(cdp.msg("Fetch.continueRequest", params, Some(session)).1)
                .await?;
            ws.send(cdp.msg("Fetch.disable", json!({}), Some(session)).1)
                .await?;
            continue;
        }
        if v["method"] == "Target.detachedFromTarget" {
            if let Some(session) = v["params"]["sessionId"].as_str() {
                sync.tabs.remove(session);
                first_request.remove(session);
            }
            continue;
        }
        if v["method"] != "Target.attachedToTarget" {
            continue;
        }
        let p = &v["params"];
        let Some(session) = p["sessionId"].as_str() else {
            continue;
        };
        let info = &p["targetInfo"];
        // Only tabs in an MCP session context; default-context tabs are out of
        // scope (and may be a human's), they're just resumed.
        if info["type"] == "page" {
            if let Some(ctx) = info["browserContextId"].as_str() {
                let rec = read_session(&sessions_dir(), ctx).await;
                let ua = rec
                    .as_ref()
                    .and_then(session_ua)
                    .or_else(|| desktop.clone());
                // A mobile session's new tabs report a touch screen from their
                // first load, so pages pick their touch behavior.
                if ua.as_ref().is_some_and(|u| u.metadata["mobile"] == true) {
                    ws.send(
                        cdp.msg(
                            "Emulation.setTouchEmulationEnabled",
                            json!({ "enabled": true, "maxTouchPoints": crate::sessions::TOUCH_POINTS }),
                            Some(session),
                        )
                        .1,
                    )
                    .await?;
                }
                if let Some(ua) = ua {
                    ws.send(
                        cdp.msg(
                            "Emulation.setUserAgentOverride",
                            json!({
                                "userAgent": ua.user_agent,
                                "userAgentMetadata": ua.metadata,
                            }),
                            Some(session),
                        )
                        .1,
                    )
                    .await?;
                    ws.send(
                        cdp.msg(
                            "Fetch.enable",
                            json!({ "patterns": [{ "resourceType": "Document", "requestStage": "Request" }] }),
                            Some(session),
                        )
                        .1,
                    )
                    .await?;
                    first_request.insert(session.to_string(), ua);
                }
                sync.tabs.insert(
                    session.to_string(),
                    Tab {
                        ctx: ctx.to_string(),
                        applied: TabApplied::default(),
                    },
                );
                if let Some(rec) = rec {
                    for m in sync.plan(&mut cdp, session, &rec) {
                        ws.send(m).await?;
                    }
                }
            }
        }
        // CDP handles a session's messages in order, so the overrides above are
        // in place before the tab runs.
        if p["waitingForDebugger"] == true {
            ws.send(
                cdp.msg("Runtime.runIfWaitingForDebugger", json!({}), Some(session))
                    .1,
            )
            .await?;
        }
    }
    Err(anyhow!("attach guard connection closed"))
}

async fn report(sync: &mut Sync, ctx: &str) {
    if let Some(rev) = sync.settled(ctx) {
        if let Err(err) = emulation::write_applied(ctx, rev).await {
            tracing::warn!(ctx, error = %err, "writing applied emulation revision failed");
        }
    }
}

/// The UA the MCP stored for this session (mobile sessions differ), if any.
fn session_ua(rec: &SessionRecord) -> Option<UaOverride> {
    Some(UaOverride {
        user_agent: rec.user_agent.clone()?,
        metadata: rec.user_agent_metadata.clone()?,
    })
}

/// The request headers with `Sec-CH-UA*` rebuilt from `ua`, if Chrome filled
/// them in with its own `HeadlessChrome` brand; `None` leaves them as sent.
fn fixed_client_hints(headers: &Value, ua: &UaOverride) -> Option<Value> {
    let headers = headers.as_object()?;
    let leaks = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("sec-ch-ua")
            && v.as_str().is_some_and(|v| v.contains("HeadlessChrome"))
    });
    if !leaks {
        return None;
    }
    let m = &ua.metadata;
    let brands = m["brands"]
        .as_array()?
        .iter()
        .map(|b| {
            format!(
                "\"{}\";v=\"{}\"",
                b["brand"].as_str().unwrap_or(""),
                b["version"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mobile = if m["mobile"] == true { "?1" } else { "?0" };
    let platform = format!("\"{}\"", m["platform"].as_str().unwrap_or(""));
    let mut out: Vec<Value> = headers
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.to_ascii_lowercase().as_str(),
                "sec-ch-ua" | "sec-ch-ua-mobile" | "sec-ch-ua-platform"
            )
        })
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();
    out.push(json!({ "name": "sec-ch-ua", "value": brands }));
    out.push(json!({ "name": "sec-ch-ua-mobile", "value": mobile }));
    out.push(json!({ "name": "sec-ch-ua-platform", "value": platform }));
    Some(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::fixed_client_hints;
    use crate::user_agent::UaOverride;
    use serde_json::json;

    #[test]
    fn rewrites_only_headless_client_hints() {
        let ua = UaOverride {
            user_agent: "Mozilla/5.0 ... Chrome/154.0.0.0 Safari/537.36".into(),
            metadata: json!({
                "brands": [
                    { "brand": "Chromium", "version": "154" },
                    { "brand": "Google Chrome", "version": "154" },
                    { "brand": "Not A(Brand", "version": "99" },
                ],
                "mobile": false,
                "platform": "Linux",
            }),
        };
        let leaking = json!({
            "Accept": "text/html",
            "sec-ch-ua": "\"Chromium\";v=\"154\", \"HeadlessChrome\";v=\"154\", \"Not A(Brand\";v=\"99\"",
            "sec-ch-ua-mobile": "?0",
            "sec-ch-ua-platform": "\"Linux\"",
        });
        let fixed = fixed_client_hints(&leaking, &ua).unwrap();
        let get = |n: &str| {
            fixed
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["name"] == n)
                .unwrap()["value"]
                .clone()
        };
        assert_eq!(
            get("sec-ch-ua"),
            "\"Chromium\";v=\"154\", \"Google Chrome\";v=\"154\", \"Not A(Brand\";v=\"99\""
        );
        assert_eq!(get("sec-ch-ua-platform"), "\"Linux\"");
        assert_eq!(get("Accept"), "text/html");
        let clean = json!({ "sec-ch-ua": "\"Chromium\";v=\"154\", \"Google Chrome\";v=\"154\"" });
        assert!(fixed_client_hints(&clean, &ua).is_none());
    }
}
