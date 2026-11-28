//! The tools on a guest session (`guest-<token>`, see [`crate::guest`]).
//!
//! The tab is in the guest's own Chrome, behind their extension, which only
//! lets through the CDP commands in extension/policy.js. So instead of the
//! chromiumoxide handlers in server.rs, which need `Runtime` (script in an
//! isolated world) for almost every step, these five tools are a separate,
//! small path that speaks raw CDP JSON through the relay, with no script at
//! all: navigate, take_snapshot (from the accessibility tree, formatted by
//! [`crate::snapshot::render_nodes`]), take_screenshot, click and type.
//! Elements are found by CSS (DOM.querySelector) or by a snapshot line such as
//! `button "Install"` (Accessibility.queryAXTree). Every other tool fails with
//! a clear error, and nothing about normal sessions changes.
use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use serde_json::{Value, json};
use std::time::Duration;

use crate::guest::{Relay, Store};

/// The tools a guest session supports.
pub const TOOLS: &[&str] = &[
    "navigate",
    "take_snapshot",
    "take_screenshot",
    "click",
    "type",
];

/// Most commands answer at once; this bounds a stuck one.
const CALL: Duration = Duration::from_secs(30);
/// A click or Enter may wait for the guest to press Allow: the extension
/// gives them 90 seconds.
const INPUT: Duration = Duration::from_secs(100);

/// Run tool `name` on the guest session for `token`.
pub async fn call(name: &str, args: &JsonObject, token: &str) -> CallToolResult {
    match run(name, args, token).await {
        Ok(r) => r,
        Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("{err:#}"))]),
    }
}

async fn run(name: &str, args: &JsonObject, token: &str) -> Result<CallToolResult> {
    if !TOOLS.contains(&name) {
        bail!(
            "`{name}` is not available for guest sessions. A guest grant allows only {}; the guest's \
             extension blocks scripts, cookies, other tabs and everything else.",
            TOOLS.join(", ")
        );
    }
    let mut relay = Relay::connect(&Store::shared(), token).await?;
    let mut out = match name {
        "navigate" => navigate(&mut relay, args).await?,
        "take_snapshot" => snapshot(&mut relay).await?,
        "take_screenshot" => screenshot(&mut relay, args, token).await?,
        "click" => click(&mut relay, args).await?,
        "type" => type_text(&mut relay, args).await?,
        _ => unreachable!("checked above"),
    };
    // Tell the agent when the extension locked it out of a page meanwhile.
    for ev in &relay.events {
        if ev.get("method").and_then(Value::as_str) == Some("Guest.navigationBlocked") {
            let origin = ev
                .pointer("/params/origin")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let why = ev
                .pointer("/params/why")
                .and_then(Value::as_str)
                .unwrap_or("");
            out.content.push(ContentBlock::text(format!(
                "The guest's extension blocked a page at {origin} ({why}); calls on that tab fail until it is back inside the grant."
            )));
        }
    }
    Ok(out)
}

fn arg_str<'a>(args: &'a JsonObject, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}
fn required<'a>(args: &'a JsonObject, k: &str) -> Result<&'a str> {
    arg_str(args, k).ok_or_else(|| anyhow!("{k} must be a non-empty string"))
}
fn unsupported(args: &JsonObject, keys: &[&str]) -> Result<()> {
    for k in keys {
        if args.contains_key(*k) {
            bail!("`{k}` is not available for guest sessions");
        }
    }
    Ok(())
}

fn text(t: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(t.into())])
}
fn with_struct(t: impl Into<String>, v: Value) -> CallToolResult {
    let mut r = text(t);
    r.structured_content = Some(v);
    r
}

/// The agent tab's URL and title, or why they aren't shown.
async fn where_now(relay: &mut Relay) -> Result<(String, String)> {
    let info = relay.call("Guest.tabInfo", json!({}), CALL).await?;
    match info.get("url").and_then(Value::as_str) {
        Some(url) => Ok((
            url.to_string(),
            info.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        None => bail!(
            "the tab is on {}, outside the grant, so the guest's extension shows nothing of it",
            info.get("outOfScope")
                .and_then(Value::as_str)
                .unwrap_or("a page")
        ),
    }
}

async fn navigate(relay: &mut Relay, args: &JsonObject) -> Result<CallToolResult> {
    unsupported(args, &["waitUntil", "text", "settleTimeout"])?;
    let url = required(args, "url")?;
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30_000)
        .min(60_000);
    let r = relay
        .call("Page.navigate", json!({ "url": url }), CALL)
        .await?;
    if let Some(e) = r
        .get("errorText")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        if e.contains("ERR_BLOCKED_BY_CLIENT") {
            bail!(
                "{url}: the guest's extension blocked it ({e}), a redirect left the grant's URLs"
            );
        }
        bail!("{url}: {e}");
    }
    relay
        .call(
            "Guest.waitForLoad",
            json!({ "timeoutMs": timeout }),
            Duration::from_millis(timeout) + CALL,
        )
        .await?;
    let (now, _) = where_now(relay).await?;
    Ok(with_struct(
        format!("Navigated to {now}"),
        json!({ "url": now }),
    ))
}

async fn snapshot(relay: &mut Relay) -> Result<CallToolResult> {
    let r = relay
        .call("Accessibility.getFullAXTree", json!({}), CALL)
        .await?;
    let nodes = serde_json::from_value::<
        Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode>,
    >(r.get("nodes").cloned().unwrap_or_else(|| json!([])))
    .map_err(|e| anyhow!("reading the accessibility tree: {e}"))?;
    Ok(text(crate::snapshot::render_nodes(&nodes)))
}

/// A selector in the form of a snapshot line: `button "Install"`.
fn ax_selector(sel: &str) -> Option<(&str, &str)> {
    let (role, rest) = sel.trim().split_once(char::is_whitespace)?;
    let name = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!role.is_empty() && role.chars().all(|c| c.is_ascii_alphabetic()) && !name.is_empty())
        .then_some((role, name))
}

/// The node `sel` names, waiting up to `timeout_ms` for it: as a node
/// reference for DOM.* calls, and a label for replies.
async fn find(relay: &mut Relay, sel: &str, timeout_ms: u64) -> Result<(Value, String)> {
    if sel.trim_start().starts_with("jev:") {
        bail!(
            "plain-language (`jev:`) selectors are not available for guest sessions: use CSS or a snapshot line like `button \"Install\"`"
        );
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        // Node ids last until the next navigation, so ask for the root each time.
        let doc = relay.call("DOM.getDocument", json!({}), CALL).await?;
        let root = doc
            .pointer("/root/nodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("the page has no document"))?;
        let found = match ax_selector(sel) {
            Some((role, name)) => {
                let r = relay
                    .call(
                        "Accessibility.queryAXTree",
                        json!({ "nodeId": root, "role": role, "accessibleName": name }),
                        CALL,
                    )
                    .await?;
                r.get("nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|n| n.get("ignored") != Some(&Value::Bool(true)))
                    .find_map(|n| n.get("backendDOMNodeId").and_then(Value::as_i64))
                    .map(|id| json!({ "backendNodeId": id }))
            }
            None => {
                let r = relay
                    .call(
                        "DOM.querySelector",
                        json!({ "nodeId": root, "selector": sel }),
                        CALL,
                    )
                    .await?;
                r.get("nodeId")
                    .and_then(Value::as_i64)
                    .filter(|&n| n > 0)
                    .map(|id| json!({ "nodeId": id }))
            }
        };
        if let Some(node) = found {
            return Ok((node, sel.to_string()));
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("`{sel}` matched nothing within {timeout_ms}ms");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// An element's border box in viewport coordinates, scrolled into view.
async fn element_box(relay: &mut Relay, node: &Value) -> Result<(f64, f64, f64, f64)> {
    relay
        .call("DOM.scrollIntoViewIfNeeded", node.clone(), CALL)
        .await?;
    let m = relay
        .call("DOM.getBoxModel", node.clone(), CALL)
        .await
        .map_err(|e| {
            anyhow!("the element isn't rendered, so it may be hidden or collapsed ({e})")
        })?;
    let quad: Vec<f64> = m
        .pointer("/model/border")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    if quad.len() != 8 {
        bail!("the element has no box (it isn't rendered)");
    }
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let (x0, x1) = (
        xs.iter().cloned().fold(f64::MAX, f64::min),
        xs.iter().cloned().fold(f64::MIN, f64::max),
    );
    let (y0, y1) = (
        ys.iter().cloned().fold(f64::MAX, f64::min),
        ys.iter().cloned().fold(f64::MIN, f64::max),
    );
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        bail!("the element has no size, so there is nothing to click or capture");
    }
    Ok((x0, y0, x1 - x0, y1 - y0))
}

async fn screenshot(relay: &mut Relay, args: &JsonObject, token: &str) -> Result<CallToolResult> {
    let full = args
        .get("fullPage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let save = args.get("save").and_then(Value::as_bool).unwrap_or(false);
    let scale = args.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
    if !(0.05..=4.0).contains(&scale) {
        bail!("`scale` must be between 0.05 and 4");
    }
    let mut params = json!({ "format": "png" });
    if let Some(sel) = arg_str(args, "selector") {
        let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(5_000);
        let (node, _) = find(relay, sel, timeout).await?;
        let (x, y, w, h) = element_box(relay, &node).await?;
        let m = relay.call("Page.getLayoutMetrics", json!({}), CALL).await?;
        let (px, py) = page_offset(&m);
        params["clip"] =
            json!({ "x": x + px, "y": y + py, "width": w, "height": h, "scale": scale });
    } else if full {
        let m = relay.call("Page.getLayoutMetrics", json!({}), CALL).await?;
        let size = m.get("cssContentSize").or_else(|| m.get("contentSize"));
        let dim = |k: &str| {
            size.and_then(|s| s.get(k))
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
        };
        params["clip"] = json!({ "x": 0, "y": 0, "width": dim("width"), "height": dim("height"), "scale": scale });
        params["captureBeyondViewport"] = json!(true);
    } else if scale != 1.0 {
        let m = relay.call("Page.getLayoutMetrics", json!({}), CALL).await?;
        let (px, py) = page_offset(&m);
        let vp = m
            .get("cssVisualViewport")
            .or_else(|| m.get("visualViewport"));
        let dim = |k: &str| {
            vp.and_then(|s| s.get(k))
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
        };
        params["clip"] = json!({ "x": px, "y": py, "width": dim("clientWidth"), "height": dim("clientHeight"), "scale": scale });
    }
    let r = relay.call("Page.captureScreenshot", params, CALL).await?;
    let b64 = r
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the guest's browser sent no image"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow!("decoding the screenshot: {e}"))?;
    if save {
        let dir = crate::logs::session_dir(
            &crate::state_dir().join("screenshots"),
            &crate::guest::session_id(token),
        )?;
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.png", crate::takeover::now_ms()));
        tokio::fs::write(&path, &bytes).await?;
        return Ok(with_struct(
            format!("Saved {} bytes PNG to {}", bytes.len(), path.display()),
            json!({ "path": path, "bytes": bytes.len() }),
        ));
    }
    Ok(CallToolResult::success(vec![
        ContentBlock::text(format!("Captured {} bytes PNG.", bytes.len())),
        ContentBlock::image(b64.to_string(), "image/png".to_string()),
    ]))
}

/// How far the page is scrolled: clip coordinates are the document's.
fn page_offset(metrics: &Value) -> (f64, f64) {
    let vp = metrics
        .get("cssVisualViewport")
        .or_else(|| metrics.get("visualViewport"));
    let at = |k: &str| {
        vp.and_then(|v| v.get(k))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    (at("pageX"), at("pageY"))
}

async fn click(relay: &mut Relay, args: &JsonObject) -> Result<CallToolResult> {
    let (before, _) = where_now(relay).await?;
    let (x, y, label) = if let Some(sel) = arg_str(args, "selector") {
        let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(5_000);
        let (node, label) = find(relay, sel, timeout).await?;
        let (x, y, w, h) = element_box(relay, &node).await?;
        (x + w / 2.0, y + h / 2.0, label)
    } else if let (Some(x), Some(y)) = (
        args.get("x").and_then(Value::as_f64),
        args.get("y").and_then(Value::as_f64),
    ) {
        (x, y, format!("({x}, {y})"))
    } else {
        bail!("click requires either `selector` or both `x` and `y`");
    };
    let mouse = |kind: &str, buttons: u8| {
        json!({ "type": kind, "x": x, "y": y, "button": if kind == "mouseMoved" { "none" } else { "left" },
                "buttons": buttons, "clickCount": if kind == "mouseMoved" { 0 } else { 1 } })
    };
    relay
        .call("Input.dispatchMouseEvent", mouse("mouseMoved", 0), CALL)
        .await?;
    // In "ask" mode this waits for the guest's Allow.
    relay
        .call("Input.dispatchMouseEvent", mouse("mousePressed", 1), INPUT)
        .await?;
    relay
        .call("Input.dispatchMouseEvent", mouse("mouseReleased", 0), CALL)
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (url, title) = where_now(relay).await?;
    let navigated = url != before;
    Ok(with_struct(
        format!("Clicked {label}; now on {url} ({title})"),
        json!({
            "url": url,
            "title": title,
            "navigated": navigated,
            "focused": "not reported for guest sessions",
        }),
    ))
}

async fn type_text(relay: &mut Relay, args: &JsonObject) -> Result<CallToolResult> {
    let sel = required(args, "selector")?;
    let typed = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("text must be a string"))?;
    let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let delay = args.get("delay").and_then(Value::as_u64).unwrap_or(0);
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(5_000);
    let (node, label) = find(relay, sel, timeout).await?;
    relay.call("DOM.focus", node, CALL).await?;
    if clear {
        let key = |kind: &str, key: &str, code: &str, vk: u8, mods: u8| {
            let mut k = json!({ "type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "modifiers": mods });
            if kind == "keyDown" && key == "a" {
                k["commands"] = json!(["selectAll"]);
            }
            k
        };
        for k in [
            key("keyDown", "a", "KeyA", 65, 2),
            key("keyUp", "a", "KeyA", 65, 2),
            key("keyDown", "Backspace", "Backspace", 8, 0),
            key("keyUp", "Backspace", "Backspace", 8, 0),
        ] {
            relay.call("Input.dispatchKeyEvent", k, CALL).await?;
        }
    }
    if delay == 0 {
        relay
            .call("Input.insertText", json!({ "text": typed }), INPUT)
            .await?;
    } else {
        for c in typed.chars() {
            relay
                .call("Input.insertText", json!({ "text": c.to_string() }), INPUT)
                .await?;
            tokio::time::sleep(Duration::from_millis(delay.min(1000))).await;
        }
    }
    let how = if clear { " after clearing it" } else { "" };
    Ok(text(format!(
        "Typed {} characters into {label}{how}",
        typed.chars().count()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_lines_work_as_selectors() {
        assert_eq!(
            ax_selector(r#"button "Install""#),
            Some(("button", "Install"))
        );
        assert_eq!(
            ax_selector(r#"link "Configure  app""#),
            Some(("link", "Configure  app"))
        );
        assert_eq!(ax_selector("form button"), None);
        assert_eq!(ax_selector(r#"div > "x""#), None);
        assert_eq!(ax_selector(r#"button """#), None);
    }

    #[tokio::test]
    async fn other_tools_are_refused_before_any_connection() {
        let args = JsonObject::new();
        // A malformed token too: the tool check comes first, so no path is built.
        let r = call("evaluate", &args, "x").await;
        assert_eq!(r.is_error, Some(true));
        let msg = format!("{:?}", r.content);
        assert!(msg.contains("not available for guest sessions"), "{msg}");
    }
}
