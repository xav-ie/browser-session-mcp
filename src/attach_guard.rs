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
use anyhow::{Context, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::chrome;
use crate::state::{read_session, sessions_dir};
use crate::user_agent::UaOverride;

/// Run until the connection drops. `desktop` is the listener's default UA for
/// sessions without a stored one.
pub async fn run(browser_url: &str, desktop: Option<UaOverride>) -> Result<()> {
    let ws_url = chrome::resolve_ws_endpoint(browser_url).await?;
    let (mut ws, _) = async_tungstenite::tokio::connect_async(ws_url)
        .await
        .context("attach guard connect")?;
    let mut next_id = 0u64;
    let mut send = |method: &str, params: Value, session: Option<&str>| {
        next_id += 1;
        let mut msg = json!({ "id": next_id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        Message::text(msg.to_string())
    };

    ws.send(send(
        "Target.setAutoAttach",
        json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true,
            "filter": [{ "type": "page", "exclude": false }, { "exclude": true }],
        }),
        None,
    ))
    .await?;
    tracing::info!("attach guard active");

    // Tabs whose first document request is still to be checked, by session.
    let mut first_request: HashMap<String, UaOverride> = HashMap::new();

    while let Some(msg) = ws.next().await {
        let msg = msg.context("attach guard read")?;
        let Ok(text) = msg.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
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
            ws.send(send("Fetch.continueRequest", params, Some(session)))
                .await?;
            ws.send(send("Fetch.disable", json!({}), Some(session)))
                .await?;
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
                let ua = session_ua(ctx).await.or_else(|| desktop.clone());
                if let Some(ua) = ua {
                    ws.send(send(
                        "Emulation.setUserAgentOverride",
                        json!({
                            "userAgent": ua.user_agent,
                            "userAgentMetadata": ua.metadata,
                        }),
                        Some(session),
                    ))
                    .await?;
                    ws.send(send(
                        "Fetch.enable",
                        json!({ "patterns": [{ "resourceType": "Document", "requestStage": "Request" }] }),
                        Some(session),
                    ))
                    .await?;
                    first_request.insert(session.to_string(), ua);
                }
            }
        }
        // CDP handles a session's messages in order, so the overrides above are
        // in place before the tab runs.
        if p["waitingForDebugger"] == true {
            ws.send(send(
                "Runtime.runIfWaitingForDebugger",
                json!({}),
                Some(session),
            ))
            .await?;
        }
    }
    Err(anyhow!("attach guard connection closed"))
}

/// The UA the MCP stored for this session (mobile sessions differ), if any.
async fn session_ua(ctx: &str) -> Option<UaOverride> {
    let rec = read_session(&sessions_dir(), ctx).await?;
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
