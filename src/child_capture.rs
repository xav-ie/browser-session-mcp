//! Record requests made by a tab's out-of-process iframes and workers.
//!
//! The listener enables `Network` on each tab's own target, which covers the
//! tab's document and its same-site frames. Cross-site iframes run in their own
//! process (site isolation) and workers run on their own thread; each is a
//! separate CDP target, so their requests never reach the tab's session. That
//! hides sandboxed third-party code such as Shopify's checkout web pixels.
//!
//! This holds its own raw CDP connection (like `attach_guard`): it attaches to
//! every tab, auto-attaches to the tab's iframe and worker targets (recursively,
//! so a worker inside an iframe is covered), enables `Network` on each while it
//! is paused at start, then resumes it. Requests are written into the owning
//! tab's current visit, so `list_network_requests` shows them alongside the
//! tab's own.
//!
//! A child target must never stay frozen, so it is resumed even when enabling
//! capture fails, and if this connection drops Chrome releases it.
use anyhow::{Context, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::chrome;
use crate::listener::{InflightRequest, Pages, decode_post_data, now_rfc3339, write_network_entry};
use crate::logs::LogWriter;

/// Run until the connection drops.
pub(crate) async fn run(browser_url: &str, writer: LogWriter, pages: Pages) -> Result<()> {
    let ws_url = chrome::resolve_ws_endpoint(browser_url).await?;
    let (mut ws, _) = async_tungstenite::tokio::connect_async(ws_url)
        .await
        .context("child capture connect")?;
    let mut next_id = 0u64;
    let mut send = |method: &str, params: Value, session: Option<&str>| {
        next_id += 1;
        let mut msg = json!({ "id": next_id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        Message::text(msg.to_string())
    };

    // Tabs only, unpaused: the attach guard is what holds new tabs.
    ws.send(send(
        "Target.setAutoAttach",
        json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": false,
            "flatten": true,
            "filter": [{ "type": "page", "exclude": false }, { "exclude": true }],
        }),
        None,
    ))
    .await?;
    tracing::info!("child capture active");

    let child_attach = json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": true,
        "flatten": true,
        "filter": [
            { "type": "iframe", "exclude": false },
            { "type": "worker", "exclude": false },
            { "exclude": true },
        ],
    });

    // CDP session -> target id of the tab it belongs to (a tab maps to itself).
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut inflight: HashMap<(String, String), InflightRequest> = HashMap::new();

    while let Some(msg) = ws.next().await {
        let msg = msg.context("child capture read")?;
        let Ok(text) = msg.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some(err) = v.get("error") {
            tracing::debug!(%err, "child capture command failed");
            continue;
        }
        let Some(method) = v["method"].as_str() else {
            continue;
        };
        let p = &v["params"];
        let from = v["sessionId"].as_str();
        match method {
            "Target.attachedToTarget" => {
                let Some(session) = p["sessionId"].as_str() else {
                    continue;
                };
                let info = &p["targetInfo"];
                match from {
                    // A tab, from the browser-wide auto-attach. Default-context
                    // tabs are outside every MCP session, so they're left alone.
                    None => {
                        let (Some(target), Some(_ctx)) =
                            (info["targetId"].as_str(), info["browserContextId"].as_str())
                        else {
                            continue;
                        };
                        owners.insert(session.to_string(), target.to_string());
                        ws.send(send(
                            "Target.setAutoAttach",
                            child_attach.clone(),
                            Some(session),
                        ))
                        .await?;
                    }
                    // An iframe or worker under a tab (or under another child).
                    Some(parent) => {
                        if let Some(tab) = owners.get(parent).cloned() {
                            owners.insert(session.to_string(), tab);
                            ws.send(send("Network.enable", json!({}), Some(session)))
                                .await?;
                            ws.send(send(
                                "Target.setAutoAttach",
                                child_attach.clone(),
                                Some(session),
                            ))
                            .await?;
                        }
                        // CDP handles a session's messages in order, so capture
                        // is on before the child runs.
                        if p["waitingForDebugger"] == true {
                            ws.send(send(
                                "Runtime.runIfWaitingForDebugger",
                                json!({}),
                                Some(session),
                            ))
                            .await?;
                        }
                    }
                }
            }
            "Target.detachedFromTarget" => {
                let Some(session) = p["sessionId"].as_str() else {
                    continue;
                };
                let Some(tab) = owners.remove(session) else {
                    continue;
                };
                // Requests still open when an iframe goes away (a beacon sent
                // on unload) are logged as they stand rather than dropped.
                let open: Vec<_> = inflight
                    .keys()
                    .filter(|(s, _)| s == session)
                    .cloned()
                    .collect();
                for key in open {
                    if let Some(inf) = inflight.remove(&key) {
                        write(&writer, &pages, &tab, inf, None).await;
                    }
                }
            }
            "Network.requestWillBeSent" => {
                let Some((session, tab)) = child_of(&owners, from) else {
                    continue;
                };
                let Some(request_id) = p["requestId"].as_str() else {
                    continue;
                };
                let key = (session.to_string(), request_id.to_string());
                let url = p["request"]["url"].as_str().unwrap_or_default();
                // A redirect reuses the requestId; log the previous hop first.
                if p.get("redirectResponse").is_some_and(|r| !r.is_null()) {
                    if let Some(mut hop) = inflight.remove(&key) {
                        hop.status = p["redirectResponse"]["status"].as_i64();
                        hop.redirected_to = Some(url.to_string());
                        write(&writer, &pages, tab, hop, None).await;
                    }
                }
                let seq = match pages.lock().await.get(tab) {
                    Some(state) => state.lock().await.current_seq,
                    None => 0,
                };
                let post_data = p["request"]["postDataEntries"].as_array().and_then(|e| {
                    decode_post_data(
                        e.iter()
                            .filter_map(|ent| ent["bytes"].as_str().map(str::to_string)),
                    )
                });
                inflight.insert(
                    key,
                    InflightRequest {
                        method: p["request"]["method"].as_str().unwrap_or_default().into(),
                        url: url.to_string(),
                        t: now_rfc3339(),
                        seq,
                        post_data,
                        ..Default::default()
                    },
                );
            }
            "Network.responseReceived" => {
                let Some((session, _)) = child_of(&owners, from) else {
                    continue;
                };
                let key = (
                    session.to_string(),
                    p["requestId"].as_str().unwrap_or_default().to_string(),
                );
                if let Some(inf) = inflight.get_mut(&key) {
                    inf.status = p["response"]["status"].as_i64();
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                let Some((session, tab)) = child_of(&owners, from) else {
                    continue;
                };
                let key = (
                    session.to_string(),
                    p["requestId"].as_str().unwrap_or_default().to_string(),
                );
                let Some(mut inf) = inflight.remove(&key) else {
                    continue;
                };
                let failure = if method == "Network.loadingFailed" {
                    inf.status = None;
                    Some(p["errorText"].as_str().unwrap_or_default().to_string())
                } else {
                    None
                };
                write(&writer, &pages, tab, inf, failure).await;
            }
            _ => {}
        }
    }
    Err(anyhow!("child capture connection closed"))
}

/// The session and owning tab of a `Network` event. Only child sessions have
/// `Network` enabled on this connection; tabs' own requests are the listener's.
fn child_of<'a>(
    owners: &'a HashMap<String, String>,
    from: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    let session = from?;
    Some((session, owners.get(session)?.as_str()))
}

async fn write(
    writer: &LogWriter,
    pages: &Pages,
    tab: &str,
    inf: InflightRequest,
    failure: Option<String>,
) {
    let Some(state) = pages.lock().await.get(tab).cloned() else {
        return;
    };
    let ctx_id = state.lock().await.ctx_id.clone();
    write_network_entry(writer, &ctx_id, tab, inf, failure).await;
}
