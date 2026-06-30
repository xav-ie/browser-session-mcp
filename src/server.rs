//! rmcp `ServerHandler` impl. Defines the tool surface and dispatches each
//! tool call to the right module.
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
};
use chromiumoxide::cdp::browser_protocol::{
    network::CookieParam,
    page::{CaptureScreenshotFormat, CreateIsolatedWorldParams, NavigateParams},
    storage::{GetCookiesParams, SetCookiesParams},
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::keys::USKEYBOARD_LAYOUT;
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use once_cell::sync::Lazy;
use regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData as McpError, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::chrome_ctx::ChromeContext;
use crate::logs::{self, LogKind, ReadOpts, SessionLogEntry};
use crate::saved_states::SavedStateStore;
use crate::sessions::Viewport;
use crate::snapshot;
use crate::takeover;

#[derive(Clone)]
pub struct BrowserSessionServer {
    ctx: ChromeContext,
    logs_dir: PathBuf,
    saved_states: SavedStateStore,
}

impl BrowserSessionServer {
    pub fn new(ctx: ChromeContext, logs_dir: PathBuf, saved_states: SavedStateStore) -> Self {
        Self {
            ctx,
            logs_dir,
            saved_states,
        }
    }
}

impl ServerHandler for BrowserSessionServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_06_18;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("browser-session-mcp", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Per-call isolated browser sessions against a shared persistent Chrome. Call open_browser_session to obtain a sessionId; pass it into every subsequent tool call. For human-takeover (request_human_takeover/await_human_takeover): show the URL, then run await_human_takeover in a BACKGROUND task that polls with a short timeout — don't block your main loop waiting on the human.".into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: tool_defs(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.unwrap_or_default();
        let name = request.name.as_ref();
        match self.dispatch(name, args).await {
            Ok(result) => Ok(result),
            Err(err) => {
                // Connection-died errors mean the next call should rebuild
                // the chromiumoxide handle from scratch.
                if looks_like_disconnect(&err) {
                    self.ctx.invalidate().await;
                }
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "{err:#}"
                ))]))
            }
        }
    }
}

impl BrowserSessionServer {
    async fn dispatch(&self, name: &str, args: JsonObject) -> Result<CallToolResult> {
        match name {
            "open_browser_session" => self.open_session(args).await,
            "close_browser_session" => self.close_session(args).await,
            "list_browser_sessions" => self.list_sessions().await,
            "new_page" => self.new_page(args).await,
            "list_pages" => self.list_pages(args).await,
            "switch_tab" => self.switch_tab(args).await,
            "close_tab" => self.close_tab(args).await,
            "navigate" => self.navigate(args).await,
            "take_screenshot" => self.take_screenshot(args).await,
            "take_snapshot" => self.take_snapshot(args).await,
            "click" => self.click(args).await,
            "type" => self.type_text(args).await,
            "press_key" => self.press_key(args).await,
            "scroll" => self.scroll(args).await,
            "move_mouse" => self.move_mouse(args).await,
            "wait_for" => self.wait_for(args).await,
            "evaluate" => self.evaluate(args).await,
            "list_visits" => self.list_visits(args).await,
            "list_console_messages" => self.list_console_messages(args).await,
            "list_network_requests" => self.list_network_requests(args).await,
            "set_stealth" => self.set_stealth(args).await,
            "get_stealth" => self.get_stealth(args).await,
            "request_human_takeover" => self.request_human_takeover(args).await,
            "await_human_takeover" => self.await_human_takeover(args).await,
            "save_browser_state" => self.save_state(args).await,
            "load_browser_state" => self.load_state(args).await,
            "list_browser_states" => self.list_states().await,
            "delete_browser_state" => self.delete_state(args).await,
            other => bail!("unknown tool: {other}"),
        }
    }

    // --- lifecycle ---

    async fn open_session(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let viewport = optional_viewport(&args, "viewport")?;
        let use_mobile = optional_bool(&args, "useMobileUA").unwrap_or(false);
        let info = sessions.open(viewport, use_mobile).await?;
        Ok(ok_text_struct(
            format!("Opened session {}", info.session_id),
            json!(info),
        ))
    }

    async fn close_session(&self, args: JsonObject) -> Result<CallToolResult> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Best-effort — closing an unknown sessionId is not an error.
        if let Ok(sm) = self.ctx.sessions().await {
            let _ = sm.close(&session_id).await;
        }
        let writer = logs::LogWriter::new(self.logs_dir.clone());
        let _ = writer.close_session(&session_id).await;
        Ok(ok_text(format!("Closed session {session_id}")))
    }

    async fn list_sessions(&self) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let list = sessions.list().await?;
        let body = if list.is_empty() {
            "No active sessions.".to_string()
        } else {
            list.iter()
                .map(|s| {
                    format!(
                        "- {}  pages={}  url={}",
                        s.session_id,
                        s.page_count,
                        s.active_url.as_deref().unwrap_or("(none)")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ok_text_struct(body, json!({ "sessions": list })))
    }

    // --- tabs ---

    async fn new_page(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let url = optional_str(&args, "url").map(str::to_string);
        let page = sessions.new_page(&session_id, url.as_deref()).await?;
        let pages = sessions.pages(&session_id).await?;
        let index = pages.len().saturating_sub(1);
        let page_url = page.url().await?.unwrap_or_default();
        Ok(ok_text_struct(
            format!("Opened tab #{index}"),
            json!({ "index": index, "url": page_url }),
        ))
    }

    async fn list_pages(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Stable order + active marker from the tab registry. Titles/urls come
        // straight from TargetInfo (no per-tab page round-trips).
        let (ordered, active) = sessions.tabs(&session_id).await?;
        let summaries: Vec<PageSummary> = ordered
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let tid = t.target_id.inner().to_string();
                PageSummary {
                    index: i,
                    url: t.url.clone(),
                    title: t.title.clone(),
                    active: active.as_deref() == Some(tid.as_str()),
                }
            })
            .collect();
        let body = if summaries.is_empty() {
            "(no pages)".to_string()
        } else {
            summaries
                .iter()
                .map(|s| {
                    let marker = if s.active { "*" } else { " " };
                    format!("{marker} {}: {}  \"{}\"", s.index, s.url, s.title)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ok_text_struct(body, json!({ "pages": summaries })))
    }

    async fn switch_tab(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let index = optional_u64(&args, "tabIndex")
            .ok_or_else(|| anyhow!("`tabIndex` (integer) is required"))?
            as usize;
        let url = sessions.switch_tab(&session_id, index).await?;
        Ok(ok_text_struct(
            format!("Switched to tab #{index} ({url})"),
            json!({ "index": index, "url": url }),
        ))
    }

    async fn close_tab(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let index = optional_u64(&args, "tabIndex")
            .ok_or_else(|| anyhow!("`tabIndex` (integer) is required"))?
            as usize;
        sessions.close_tab(&session_id, index).await?;
        Ok(ok_text(format!("Closed tab #{index}")))
    }

    async fn navigate(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let url = required_str(&args, "url")?.to_string();
        let timeout_ms = optional_u64(&args, "timeout");
        let page = sessions.active_page(&session_id).await?;
        let nav = NavigateParams::builder()
            .url(url.clone())
            .build()
            .map_err(|e| anyhow!("NavigateParams: {e}"))?;
        // chromiumoxide's goto already resolves after the load event; no
        // separate wait_for_navigation needed.
        let goto_fut = page.goto(nav);
        if let Some(ms) = timeout_ms {
            tokio::time::timeout(Duration::from_millis(ms), goto_fut)
                .await
                .map_err(|_| anyhow!("navigation timed out after {ms}ms"))?
                .context("Page.navigate")?;
        } else {
            goto_fut.await.context("Page.navigate")?;
        }
        let resolved_url = page.url().await?.unwrap_or(url);
        Ok(ok_text_struct(
            format!("Navigated to {resolved_url}"),
            json!({ "url": resolved_url }),
        ))
    }

    // --- capture ---

    async fn take_screenshot(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let full_page = optional_bool(&args, "fullPage").unwrap_or(false);
        let page = sessions.active_page(&session_id).await?;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .full_page(full_page)
            .build();
        let bytes = page
            .screenshot(params)
            .await
            .context("Page.captureScreenshot")?;
        let len = bytes.len();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(CallToolResult::success(vec![
            Content::text(format!("Captured {len} bytes PNG.")),
            Content::image(b64, "image/png".to_string()),
        ]))
    }

    async fn take_snapshot(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let tree = snapshot::snapshot(&page).await?;
        Ok(ok_text(tree))
    }

    // --- interact ---

    async fn click(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        // CDP Input (trusted events): reliably fires handlers AND grants user
        // activation, so it can open popups/new tabs that a synthetic JS
        // `.click()` cannot. Target a selector's center or explicit x/y.
        let (x, y, label) = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            wait_for_selector(&page, sel, timeout).await?;
            let (x, y) = element_center(&page, sel).await?;
            (x, y, sel.to_string())
        } else if let (Some(x), Some(y)) = (optional_f64(&args, "x"), optional_f64(&args, "y")) {
            (x, y, format!("({x}, {y})"))
        } else {
            bail!("click requires either `selector` or both `x` and `y`");
        };
        page.click(Point::new(x, y))
            .await
            .with_context(|| format!("clicking {label}"))?;
        Ok(ok_text(format!("Clicked {label}")))
    }

    async fn press_key(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let key = required_str(&args, "key")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        if let Some(sel) = optional_str(&args, "selector") {
            // Focus the element, then press (e.g. type into a field then Enter).
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            wait_for_selector(&page, sel, timeout).await?;
            page.find_element(sel.to_string())
                .await
                .with_context(|| format!("locating {sel}"))?
                .press_key(key.as_str())
                .await
                .with_context(|| format!("pressing {key}"))?;
        } else {
            dispatch_key(&page, &key).await?;
        }
        Ok(ok_text(format!("Pressed {key}")))
    }

    async fn scroll(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let dx = optional_f64(&args, "deltaX").unwrap_or(0.0);
        let dy = optional_f64(&args, "deltaY").unwrap_or(600.0);
        let x = optional_f64(&args, "x").unwrap_or(50.0);
        let y = optional_f64(&args, "y").unwrap_or(50.0);
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(x)
            .y(y)
            .delta_x(dx)
            .delta_y(dy)
            .build()
            .map_err(|e| anyhow!("scroll: {e}"))?;
        page.execute(params)
            .await
            .context("Input.dispatchMouseEvent")?;
        Ok(ok_text(format!("Scrolled (deltaX={dx}, deltaY={dy})")))
    }

    async fn move_mouse(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let x = optional_f64(&args, "x").ok_or_else(|| anyhow!("`x` is required"))?;
        let y = optional_f64(&args, "y").ok_or_else(|| anyhow!("`y` is required"))?;
        page.move_mouse(Point::new(x, y))
            .await
            .context("Input.dispatchMouseEvent (move)")?;
        Ok(ok_text(format!("Moved mouse to ({x}, {y})")))
    }

    async fn type_text(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = required_str(&args, "selector")?.to_string();
        let text = required_str(&args, "text")?.to_string();
        let clear = optional_bool(&args, "clear").unwrap_or(false);
        let page = sessions.active_page(&session_id).await?;
        wait_for_selector(&page, &selector, 5_000).await?;
        let element = page
            .find_element(selector.clone())
            .await
            .with_context(|| format!("locating {selector}"))?;
        if clear {
            let _ = element
                .call_js_fn(
                    "function() { if (this.value !== undefined) this.value = ''; }",
                    false,
                )
                .await;
        }
        element.click().await.ok();
        element
            .type_str(text.as_str())
            .await
            .with_context(|| format!("typing into {selector}"))?;
        Ok(ok_text(format!(
            "Typed {} chars into {selector}",
            text.chars().count()
        )))
    }

    async fn wait_for(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = optional_str(&args, "selector").map(str::to_string);
        let text = optional_str(&args, "text").map(str::to_string);
        let timeout = optional_u64(&args, "timeout").unwrap_or(10_000);
        if selector.is_none() && text.is_none() {
            bail!("wait_for requires either `selector` or `text`.");
        }
        let page = sessions.active_page(&session_id).await?;
        if let Some(s) = selector {
            wait_for_selector(&page, &s, timeout).await?;
            return Ok(ok_text(format!("Matched selector {s}")));
        }
        let needle = text.unwrap();
        let body = format!(
            "function() {{ return document.body && document.body.innerText && document.body.innerText.includes({}); }}",
            serde_json::to_string(&needle).unwrap()
        );
        wait_until(
            || async {
                let v = iso_eval_function(&page, &body).await?;
                Ok(v.as_bool().unwrap_or(false))
            },
            timeout,
        )
        .await?;
        Ok(ok_text(format!("Matched text \"{needle}\"")))
    }

    async fn evaluate(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let expression = required_str(&args, "expression")?.to_string();
        // Default: the active (most-recently-listed) tab. With `tabIndex`, target
        // a specific tab by its `list_pages` index — needed because the active-tab
        // heuristic can get stuck on a background tab (e.g. a Tag Assistant tab
        // that keeps re-creating its target), making other tabs unreachable.
        let page = match optional_u64(&args, "tabIndex") {
            Some(idx) => sessions.page_at(&session_id, idx as usize).await?,
            None => sessions.active_page(&session_id).await?,
        };
        // Word-bounded so `document.returnValue` etc. don't false-match.
        static RETURN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\breturn\b").unwrap());
        let body = if RETURN_RE.is_match(&expression) {
            expression.clone()
        } else {
            format!("return ({expression});")
        };
        let wrapped = format!("async function() {{ {body} }}");
        // Main world by default — so `window.dataLayer`, `__NEXT_DATA__`, and
        // other page-author globals are readable. Only stealth sessions
        // (console-capture toggled OFF → stealth flag present) fall back to an
        // isolated world, which is invisible to page scripts but can't see those
        // globals. The session already exposes Runtime via the listener when not
        // in stealth, so main-world execution costs no extra detectability there.
        let stealth = tokio::fs::try_exists(takeover::stealth_path(&session_id))
            .await
            .unwrap_or(false);
        let value = if stealth {
            iso_eval_function(&page, &wrapped).await
        } else {
            main_world_eval(&page, &wrapped).await
        }
        .context("evaluate")?;
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(ok_text_struct(text, json!({ "result": value })))
    }

    // --- human takeover ---

    async fn request_human_takeover(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        // How long the link stays valid. The URL is a bearer credential (it
        // grants live input to the session), so keep the default tight.
        // Default 5 min, capped at 30.
        let ttl_ms = optional_u64(&args, "ttl").unwrap_or(300_000).min(1_800_000);
        let base = takeover::base_url().ok_or_else(|| {
            anyhow!("TAKEOVER_BASE_URL is not configured — the takeover daemon/route isn't set up on this host")
        })?;
        // Resolve the session's active page; the human drives this exact target.
        let target_id = sessions.active_target_id(&session_id).await?;
        let token = takeover::mint_token()?;
        let expires_at_ms = takeover::expiry_ms(ttl_ms);
        takeover::write_ticket(
            &token,
            &takeover::Ticket {
                session_id: session_id.clone(),
                target_id,
                expires_at_ms,
            },
        )
        .await?;
        let url = format!("{base}/takeover/{token}");
        Ok(ok_text_struct(
            format!(
                "Human takeover ready. Show this URL to the user so they can log in themselves. \
                 Then run await_human_takeover(token=\"{token}\") in a BACKGROUND task that polls \
                 with a short timeout until completed — do NOT block your main loop; the human may \
                 take several minutes:\n{url}"
            ),
            json!({ "url": url, "token": token, "expiresAtMs": expires_at_ms }),
        ))
    }

    async fn await_human_takeover(&self, args: JsonObject) -> Result<CallToolResult> {
        let token = required_str(&args, "token")?.to_string();
        // How long to block. Default 5 min; capped at 30 to match the max link TTL.
        let timeout_ms = optional_u64(&args, "timeout")
            .unwrap_or(300_000)
            .min(1_800_000);
        let completed = takeover::wait_for_done(&token, Duration::from_millis(timeout_ms)).await;
        if completed {
            takeover::cleanup(&token).await;
        }
        let body = if completed {
            "Human signalled done — the session is authenticated; resuming."
        } else {
            "Timed out waiting for the human to finish. The link may still be valid; call await_human_takeover again or issue a new request_human_takeover."
        };
        Ok(ok_text_struct(
            body.to_string(),
            json!({ "completed": completed, "token": token }),
        ))
    }

    // --- per-visit logs ---

    async fn list_visits(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let _ = sessions.context_id_for(&session_id).await?;
        let visits = logs::read_visits(&self.logs_dir, &session_id).await?;
        let body = if visits.is_empty() {
            "(no visits recorded)".to_string()
        } else {
            visits
                .iter()
                .map(|v| {
                    let short = if v.target_id.len() > 8 {
                        &v.target_id[..8]
                    } else {
                        v.target_id.as_str()
                    };
                    format!("seq={}  target={short}  {}  {}", v.seq, v.opened_at, v.url)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ok_text_struct(body, json!({ "visits": visits })))
    }

    async fn list_console_messages(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let _ = sessions.context_id_for(&session_id).await?;
        let visit = optional_u32(&args, "visit");
        let limit = optional_usize(&args, "limit").unwrap_or(500);
        let entries = logs::read_session_logs(
            &self.logs_dir,
            &session_id,
            ReadOpts {
                kind: Some(LogKind::Console),
                limit: Some(limit),
                visit,
            },
        )
        .await?;
        let body = if entries.is_empty() {
            "(no console messages)".to_string()
        } else {
            entries
                .iter()
                .filter_map(|e| match e {
                    SessionLogEntry::Console(c) => Some(format!("[{}] {}", c.ty, c.text)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let json_entries: Vec<Value> = entries
            .iter()
            .filter_map(|e| match e {
                SessionLogEntry::Console(c) => serde_json::to_value(c).ok(),
                _ => None,
            })
            .collect();
        Ok(ok_text_struct(body, json!({ "messages": json_entries })))
    }

    async fn list_network_requests(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let _ = sessions.context_id_for(&session_id).await?;
        let visit = optional_u32(&args, "visit");
        let limit = optional_usize(&args, "limit").unwrap_or(500);
        let entries = logs::read_session_logs(
            &self.logs_dir,
            &session_id,
            ReadOpts {
                kind: Some(LogKind::Network),
                limit: Some(limit),
                visit,
            },
        )
        .await?;
        let body = if entries.is_empty() {
            "(no network requests)".to_string()
        } else {
            entries
                .iter()
                .filter_map(|e| match e {
                    SessionLogEntry::Network(n) => {
                        let status = if let Some(f) = &n.failure {
                            format!("FAIL:{f}")
                        } else if let Some(s) = n.status {
                            s.to_string()
                        } else {
                            "pending".to_string()
                        };
                        let body = match &n.post_data {
                            Some(b) if b.chars().count() > 1000 => {
                                format!("  body={}…", b.chars().take(1000).collect::<String>())
                            }
                            Some(b) => format!("  body={b}"),
                            None => String::new(),
                        };
                        Some(format!("{} {} [{status}]{body}", n.method, n.url))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let json_entries: Vec<Value> = entries
            .iter()
            .filter_map(|e| match e {
                SessionLogEntry::Network(n) => serde_json::to_value(n).ok(),
                _ => None,
            })
            .collect();
        Ok(ok_text_struct(body, json!({ "requests": json_entries })))
    }

    // --- stealth ---

    async fn set_stealth(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Validate the session exists (resolves from Chrome).
        let _ = sessions.context_id_for(&session_id).await?;
        let enabled = optional_bool(&args, "enabled")
            .ok_or_else(|| anyhow!("`enabled` (boolean) is required"))?;
        takeover::set_stealth(&session_id, enabled).await?;
        let msg = if enabled {
            "Stealth ON: listener Runtime disabled — no CDP Runtime.enable tell (passes CDP-based bot detection), but console capture is off and `evaluate` runs in an isolated world (can't read page globals like window.dataLayer). Load/navigate pages in this mode to pass bot gates."
        } else {
            "Stealth OFF: listener Runtime enabled — console capture on and `evaluate` runs in the main world (reads window.dataLayer etc.), but the CDP Runtime.enable tell is present (may be bot-flagged on a fresh page load)."
        };
        Ok(ok_text_struct(
            msg.to_string(),
            json!({ "stealth": enabled }),
        ))
    }

    async fn get_stealth(&self, args: JsonObject) -> Result<CallToolResult> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        let enabled = takeover::is_stealth(&session_id).await;
        Ok(ok_text_struct(
            format!("stealth = {enabled}"),
            json!({ "stealth": enabled }),
        ))
    }

    // --- saved browser states ---

    async fn save_state(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let name = required_str(&args, "name")?.to_string();
        let ctx_id = sessions.context_id_for(&session_id).await?;
        let result = sessions
            .browser()
            .execute(
                GetCookiesParams::builder()
                    .browser_context_id(ctx_id)
                    .build(),
            )
            .await
            .context("Storage.getCookies")?;
        let cookies_json: Vec<Value> = result
            .result
            .cookies
            .clone()
            .into_iter()
            .map(|c| serde_json::to_value(&c).unwrap_or(Value::Null))
            .collect();
        let state = self.saved_states.save(&name, cookies_json.clone()).await?;
        Ok(ok_text_struct(
            format!("Saved {} cookies as \"{name}\".", state.cookies.len()),
            json!({
                "name": state.name,
                "savedAt": state.saved_at,
                "cookieCount": state.cookies.len(),
            }),
        ))
    }

    async fn load_state(&self, args: JsonObject) -> Result<CallToolResult> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let name = required_str(&args, "name")?.to_string();
        let ctx_id = sessions.context_id_for(&session_id).await?;
        let state = self.saved_states.load(&name).await?;
        if !state.cookies.is_empty() {
            // Storage.setCookies expects CookieParam; the saved cookies came
            // from Network.Cookie. Strip fields that don't exist on
            // CookieParam (session, size) and the -1.0 expires sentinel for
            // session cookies (CookieParam treats absent expires as session,
            // and some Chrome versions reject the literal -1).
            let mut params: Vec<CookieParam> = Vec::with_capacity(state.cookies.len());
            for raw in &state.cookies {
                let mut cleaned = raw.clone();
                if let Some(obj) = cleaned.as_object_mut() {
                    obj.remove("session");
                    obj.remove("size");
                    if obj.get("expires").and_then(|v| v.as_f64()) == Some(-1.0) {
                        obj.remove("expires");
                    }
                }
                match serde_json::from_value::<CookieParam>(cleaned) {
                    Ok(p) => params.push(p),
                    Err(err) => tracing::warn!(error = %err, "skipping unparsable saved cookie"),
                }
            }
            sessions
                .browser()
                .execute(
                    SetCookiesParams::builder()
                        .cookies(params)
                        .browser_context_id(ctx_id)
                        .build()
                        .map_err(|e| anyhow!("SetCookiesParams: {e}"))?,
                )
                .await
                .context("Storage.setCookies")?;
        }
        Ok(ok_text_struct(
            format!("Loaded {} cookies from \"{name}\".", state.cookies.len()),
            json!({ "name": name, "cookieCount": state.cookies.len() }),
        ))
    }

    async fn list_states(&self) -> Result<CallToolResult> {
        let list = self.saved_states.list().await?;
        let body = if list.is_empty() {
            "(no saved states)".to_string()
        } else {
            list.iter()
                .map(|s| {
                    format!(
                        "- {}  cookies={}  saved={}",
                        s.name, s.cookie_count, s.saved_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ok_text_struct(body, json!({ "states": list })))
    }

    async fn delete_state(&self, args: JsonObject) -> Result<CallToolResult> {
        let name = required_str(&args, "name")?.to_string();
        let removed = self.saved_states.delete(&name).await?;
        let msg = if removed {
            format!("Deleted \"{name}\".")
        } else {
            format!("No saved state named \"{name}\".")
        };
        Ok(ok_text_struct(
            msg,
            json!({ "name": name, "removed": removed }),
        ))
    }
}

// ---- helpers --------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PageSummary {
    index: usize,
    url: String,
    title: String,
    active: bool,
}

fn ok_text(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn ok_text_struct(text: impl Into<String>, structured: Value) -> CallToolResult {
    let mut r = CallToolResult::success(vec![Content::text(text.into())]);
    r.structured_content = Some(structured);
    r
}

fn looks_like_disconnect(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("connection closed")
        || msg.contains("disconnected")
        || msg.contains("connection refused")
        || msg.contains("broken pipe")
        || msg.contains("websocket")
}

fn required_str<'a>(args: &'a JsonObject, field: &str) -> Result<&'a str> {
    args.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{field} must be a non-empty string"))
}

fn optional_str<'a>(args: &'a JsonObject, field: &str) -> Option<&'a str> {
    args.get(field).and_then(|v| v.as_str())
}

fn optional_bool(args: &JsonObject, field: &str) -> Option<bool> {
    args.get(field).and_then(|v| v.as_bool())
}

fn optional_u32(args: &JsonObject, field: &str) -> Option<u32> {
    args.get(field)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
}

fn optional_u64(args: &JsonObject, field: &str) -> Option<u64> {
    args.get(field).and_then(|v| v.as_u64())
}

fn optional_f64(args: &JsonObject, field: &str) -> Option<f64> {
    args.get(field).and_then(|v| v.as_f64())
}

fn optional_usize(args: &JsonObject, field: &str) -> Option<usize> {
    args.get(field)
        .and_then(|v| v.as_u64())
        .and_then(|n| usize::try_from(n).ok())
}

fn optional_viewport(args: &JsonObject, field: &str) -> Result<Option<Viewport>> {
    let Some(v) = args.get(field) else {
        return Ok(None);
    };
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("{field} must be an object {{width, height}}"))?;
    let width = obj
        .get("width")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("{field}.width must be a positive integer"))?;
    let height = obj
        .get("height")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("{field}.height must be a positive integer"))?;
    Ok(Some(Viewport { width, height }))
}

/// Evaluate a JS *function declaration* in a fresh isolated world and return
/// its JSON result.
///
/// Our forked chromiumoxide deliberately never sends `Runtime.enable` (the
/// primary CDP automation tell anti-bot systems watch for), so no execution
/// contexts are tracked and a bare `Runtime.callFunctionOn` would have no
/// context to run in. We get one the stealthy way: `Page.createIsolatedWorld`
/// returns an execution-context id directly in its response, and
/// `callFunctionOn` against that id works without `Runtime.enable`. Running in
/// an isolated world also keeps the probe invisible to page scripts.
/// Trade-off: page-defined `window` globals aren't visible here — but DOM,
/// `navigator`, `document`, `location` all are, which covers our use.
async fn iso_eval_function(page: &Page, function_declaration: &str) -> Result<Value> {
    let frame_id = page
        .mainframe()
        .await?
        .ok_or_else(|| anyhow!("no main frame to evaluate in"))?;
    let world = page
        .execute(
            CreateIsolatedWorldParams::builder()
                .frame_id(frame_id)
                .grant_univeral_access(true)
                .build()
                .map_err(|e| anyhow!("CreateIsolatedWorld: {e}"))?,
        )
        .await?;
    let call = CallFunctionOnParams::builder()
        .function_declaration(function_declaration)
        .execution_context_id(world.result.execution_context_id)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| anyhow!("CallFunctionOn builder: {e}"))?;
    let resp = page.execute(call).await?;
    if let Some(exc) = resp.result.exception_details.as_ref() {
        bail!("JS exception: {}", exc.text);
    }
    Ok(resp.result.result.value.clone().unwrap_or(Value::Null))
}

/// Evaluate a JS function *declaration* in the page's MAIN world and return its
/// JSON result.
///
/// Unlike [`iso_eval_function`], this shares the page's JavaScript scope, so it
/// can read page-author globals (`window.dataLayer`, `window.__NEXT_DATA__`,
/// etc.). `Runtime.evaluate` with no `contextId` targets the default (main)
/// execution context and — crucially — does NOT require `Runtime.enable`, so it
/// carries no CDP automation tell. Trade-off vs the isolated world: main-world
/// execution is in principle observable by page scripts (a `mainWorldExecution`
/// trap), so callers use this only on non-stealth sessions.
async fn main_world_eval(page: &Page, function_declaration: &str) -> Result<Value> {
    let wrapped = format!("({function_declaration})()");
    let resp = page
        .execute(
            EvaluateParams::builder()
                .expression(wrapped)
                .await_promise(true)
                .return_by_value(true)
                .build()
                .map_err(|e| anyhow!("Evaluate builder: {e}"))?,
        )
        .await?;
    if let Some(exc) = resp.result.exception_details.as_ref() {
        bail!("JS exception: {}", exc.text);
    }
    Ok(resp.result.result.value.clone().unwrap_or(Value::Null))
}

/// Center of an element in viewport CSS pixels, after scrolling it into view.
/// Evaluated in an isolated world so it works regardless of stealth.
async fn element_center(page: &Page, selector: &str) -> Result<(f64, f64)> {
    let decl = format!(
        "function() {{ const el = document.querySelector({sel}); if (!el) return null; \
         el.scrollIntoView({{block:'center',inline:'center'}}); const r = el.getBoundingClientRect(); \
         return [r.left + r.width/2, r.top + r.height/2]; }}",
        sel = serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string())
    );
    let v = iso_eval_function(page, &decl).await?;
    let arr = v
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| anyhow!("element `{selector}` not found or not visible"))?;
    let x = arr[0].as_f64().ok_or_else(|| anyhow!("bad x coord"))?;
    let y = arr[1].as_f64().ok_or_else(|| anyhow!("bad y coord"))?;
    Ok((x, y))
}

/// Press a key at the page level (on whatever's focused) via CDP Input,
/// looking the key up in chromiumoxide's US keyboard layout. Printable keys
/// send `text` with a `keyDown`; named keys (Enter, Tab, Escape, ArrowDown…)
/// use `rawKeyDown`. keyDown + keyUp are dispatched.
async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    let def = USKEYBOARD_LAYOUT.iter().find(|k| k.key == key).ok_or_else(|| {
        anyhow!("unknown key `{key}` (use names like Enter, Tab, Escape, ArrowDown, Backspace, or a single char)")
    })?;
    let text: Option<&str> = def.text.or(if def.key.chars().count() == 1 {
        Some(def.key)
    } else {
        None
    });
    let down_type = if text.is_some() {
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    let mut down = DispatchKeyEventParams::builder()
        .r#type(down_type)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code);
    if let Some(t) = text {
        down = down.text(t);
    }
    page.execute(down.build().map_err(|e| anyhow!("key down: {e}"))?)
        .await
        .context("Input.dispatchKeyEvent (down)")?;
    let up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code)
        .build()
        .map_err(|e| anyhow!("key up: {e}"))?;
    page.execute(up)
        .await
        .context("Input.dispatchKeyEvent (up)")?;
    Ok(())
}

async fn wait_for_selector(page: &Page, selector: &str, timeout_ms: u64) -> Result<()> {
    let sel = selector.to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if page.find_element(sel.clone()).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for selector {sel}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_until<F, Fut>(mut check: F, timeout_ms: u64) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if check().await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- tool defs ------------------------------------------------------------

fn tool_defs() -> Vec<Tool> {
    let str_t = obj(&[("type", Value::String("string".into()))]);
    let bool_t = obj(&[("type", Value::String("boolean".into()))]);
    let pos_int_t = obj(&[
        ("type", Value::String("integer".into())),
        ("minimum", json!(1)),
    ]);
    let nonneg_int_t = obj(&[
        ("type", Value::String("integer".into())),
        ("minimum", json!(0)),
    ]);
    let num_t = obj(&[("type", Value::String("number".into()))]);
    let url_t = obj(&[
        ("type", Value::String("string".into())),
        ("format", Value::String("uri".into())),
    ]);
    let viewport_t = obj(&[
        ("type", Value::String("object".into())),
        (
            "properties",
            json!({
                "width": pos_int_t.clone(),
                "height": pos_int_t.clone(),
            }),
        ),
        ("required", json!(["width", "height"])),
        ("additionalProperties", Value::Bool(false)),
    ]);
    vec![
        tool(
            "open_browser_session",
            "Open a new isolated browser session and return its sessionId. Pass this id to every subsequent tool call. Sessions are full BrowserContexts — each has its own cookies, storage, and tabs.",
            object_schema(&[("viewport", &viewport_t), ("useMobileUA", &bool_t)], &[]),
        ),
        tool(
            "close_browser_session",
            "Close a browser session. Releases its tabs, cookies, and storage. Idempotent.",
            object_schema(&[("sessionId", &str_t)], &["sessionId"]),
        ),
        tool(
            "list_browser_sessions",
            "List active browser sessions on the underlying Chrome.",
            empty_obj_schema(),
        ),
        tool(
            "new_page",
            "Open a new tab in the session. Subsequent tool calls target this new tab.",
            object_schema(&[("sessionId", &str_t), ("url", &url_t)], &["sessionId"]),
        ),
        tool(
            "list_pages",
            "List all tabs open in the session, in stable order. The active tab (the one navigate/evaluate/screenshot/click target by default) is marked with `*` in the text and `active: true` in the structured output. Tab indices are stable across calls.",
            object_schema(&[("sessionId", &str_t)], &["sessionId"]),
        ),
        tool(
            "switch_tab",
            "Make the tab at `tabIndex` (a list_pages index) the active tab — subsequent navigate/evaluate/screenshot/click/type target it — and bring it to the front in Chrome (also updates the human-takeover screencast).",
            object_schema(
                &[("sessionId", &str_t), ("tabIndex", &nonneg_int_t)],
                &["sessionId", "tabIndex"],
            ),
        ),
        tool(
            "close_tab",
            "Close the tab at `tabIndex` (a list_pages index). The session and other tabs stay open; the active tab falls back to the last remaining tab.",
            object_schema(
                &[("sessionId", &str_t), ("tabIndex", &nonneg_int_t)],
                &["sessionId", "tabIndex"],
            ),
        ),
        tool(
            "navigate",
            "Navigate the session's active page to a URL. Resolves after the load event.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("url", &url_t),
                    ("timeout", &pos_int_t),
                ],
                &["sessionId", "url"],
            ),
        ),
        tool(
            "take_screenshot",
            "Capture a PNG screenshot of the session's active page. Returns the image as base64 embedded in the response.",
            object_schema(
                &[("sessionId", &str_t), ("fullPage", &bool_t)],
                &["sessionId"],
            ),
        ),
        tool(
            "take_snapshot",
            "Capture the accessibility tree of the session's active page as indented text.",
            object_schema(&[("sessionId", &str_t)], &["sessionId"]),
        ),
        tool(
            "click",
            "Click via real CDP mouse input (trusted events — fires handlers reliably and grants user activation, so it can open popups/new tabs that a JS click can't). Provide a CSS `selector` (clicks its center, scrolled into view) OR explicit `x`/`y` viewport coordinates.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("selector", &str_t),
                    ("x", &num_t),
                    ("y", &num_t),
                    ("timeout", &pos_int_t),
                ],
                &["sessionId"],
            ),
        ),
        tool(
            "type",
            "Type text into the first element matching the CSS selector.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("selector", &str_t),
                    ("text", &str_t),
                    ("delay", &nonneg_int_t),
                    ("clear", &bool_t),
                ],
                &["sessionId", "selector", "text"],
            ),
        ),
        tool(
            "press_key",
            "Press a key via CDP keyboard input. `key` is a name (Enter, Tab, Escape, Backspace, ArrowDown, …) or a single character. With a `selector`, focuses that element first (e.g. type into a field then press Enter); without, presses on whatever is focused.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("key", &str_t),
                    ("selector", &str_t),
                    ("timeout", &pos_int_t),
                ],
                &["sessionId", "key"],
            ),
        ),
        tool(
            "scroll",
            "Scroll via a real mouse-wheel event. `deltaY` (default 600; negative scrolls up) and `deltaX` (default 0) are pixels; `x`/`y` are the wheel position (default 50,50). Useful to trigger lazy-loaded content and to generate human-like behavioral signals.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("deltaY", &num_t),
                    ("deltaX", &num_t),
                    ("x", &num_t),
                    ("y", &num_t),
                ],
                &["sessionId"],
            ),
        ),
        tool(
            "move_mouse",
            "Move the mouse to viewport coordinates `x`,`y` via CDP input (trusted move event) — triggers hover handlers and contributes human-like behavioral telemetry.",
            object_schema(
                &[("sessionId", &str_t), ("x", &num_t), ("y", &num_t)],
                &["sessionId", "x", "y"],
            ),
        ),
        tool(
            "wait_for",
            "Wait for a CSS selector to appear, or for a text string to be present in the page body.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("selector", &str_t),
                    ("text", &str_t),
                    ("timeout", &pos_int_t),
                ],
                &["sessionId"],
            ),
        ),
        tool(
            "evaluate",
            "Run a JavaScript expression in the page and return its value. Wrap with `return ...` for multi-statement bodies. Runs in the page's main world (can read page globals like window.dataLayer); on stealth sessions (console-capture off) it runs in an isolated world instead, which can't see page-defined globals. By default targets the active tab; pass `tabIndex` (a list_pages index) to evaluate in a specific tab.",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("expression", &str_t),
                    ("tabIndex", &nonneg_int_t),
                ],
                &["sessionId", "expression"],
            ),
        ),
        tool(
            "list_visits",
            "List page visits in this session, oldest first. Each visit is one top-level navigation in one tab.",
            object_schema(&[("sessionId", &str_t)], &["sessionId"]),
        ),
        tool(
            "list_console_messages",
            "List console messages emitted by the session. Returns up to `limit` most-recent entries (default 500).",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("visit", &pos_int_t),
                    ("limit", &pos_int_t),
                ],
                &["sessionId"],
            ),
        ),
        tool(
            "list_network_requests",
            "List network requests made by the session. Returns up to `limit` most-recent entries (default 500).",
            object_schema(
                &[
                    ("sessionId", &str_t),
                    ("visit", &pos_int_t),
                    ("limit", &pos_int_t),
                ],
                &["sessionId"],
            ),
        ),
        tool(
            "set_stealth",
            "Toggle stealth for a session. `enabled: true` disables the CDP Runtime domain (removes the Runtime.enable automation tell that anti-bot systems like Cloudflare detect) — load/navigate pages in this mode to pass CDP-based bot gates; the cost is no console capture and `evaluate` runs in an isolated world (can't read page globals like window.dataLayer). `enabled: false` re-enables Runtime: console capture + main-world `evaluate` (reads page globals) return, but the CDP tell is present. Typical flow: set_stealth(true) → navigate → set_stealth(false) → evaluate. Replaces the old console-capture toggle that required a takeover token.",
            object_schema(
                &[("sessionId", &str_t), ("enabled", &bool_t)],
                &["sessionId", "enabled"],
            ),
        ),
        tool(
            "get_stealth",
            "Return whether stealth is currently enabled for a session ({ stealth: bool }).",
            object_schema(&[("sessionId", &str_t)], &["sessionId"]),
        ),
        tool(
            "request_human_takeover",
            "Hand the session's active page to a human to log in themselves (passwords/passkeys the agent must not see). Returns a URL to show the user and a token. Does NOT block. STRONGLY ADVISED: after showing the URL, run await_human_takeover in a BACKGROUND task (subagent/job) — not your main loop — because the human may take many minutes and you should stay responsive. `ttl` (ms, default 300000, max 1800000) bounds how long the link (a bearer credential) is valid.",
            object_schema(
                &[("sessionId", &str_t), ("ttl", &pos_int_t)],
                &["sessionId"],
            ),
        ),
        tool(
            "await_human_takeover",
            "Wait for the human to click Done in the takeover page. Returns { completed } — true if they finished, false on timeout. STRONGLY ADVISED: run this in a BACKGROUND task and POLL — call it in a loop with a SHORT `timeout` (e.g. 4000ms) until completed:true — rather than one long blocking call, which can exceed your MCP client's request timeout and make you miss the Done signal. The human may take several minutes; a background poller lets you be notified and resume. `timeout` ms: default 300000, max 1800000. Pass the token from request_human_takeover. On completion the session is authenticated and you can continue.",
            object_schema(&[("token", &str_t), ("timeout", &pos_int_t)], &["token"]),
        ),
        tool(
            "save_browser_state",
            "Save the session's cookies under a name so a future session can load them and resume without logging in again.",
            object_schema(
                &[("sessionId", &str_t), ("name", &str_t)],
                &["sessionId", "name"],
            ),
        ),
        tool(
            "load_browser_state",
            "Load a previously saved set of cookies into this session.",
            object_schema(
                &[("sessionId", &str_t), ("name", &str_t)],
                &["sessionId", "name"],
            ),
        ),
        tool(
            "list_browser_states",
            "List all saved browser states.",
            empty_obj_schema(),
        ),
        tool(
            "delete_browser_state",
            "Delete a saved browser state by name.",
            object_schema(&[("name", &str_t)], &["name"]),
        ),
    ]
}

fn tool(name: &'static str, description: &'static str, schema: JsonObject) -> Tool {
    Tool::new(name.to_string(), description.to_string(), Arc::new(schema))
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).into(), v.clone());
    }
    Value::Object(m)
}

fn empty_obj_schema() -> JsonObject {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("object".into()));
    m.insert("properties".into(), Value::Object(Map::new()));
    m.insert("additionalProperties".into(), Value::Bool(false));
    m
}

fn object_schema(props: &[(&str, &Value)], required: &[&str]) -> JsonObject {
    let mut properties = Map::new();
    for (k, v) in props {
        properties.insert((*k).into(), (*v).clone());
    }
    let mut m = Map::new();
    m.insert("type".into(), Value::String("object".into()));
    m.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        m.insert(
            "required".into(),
            Value::Array(
                required
                    .iter()
                    .map(|s| Value::String((*s).into()))
                    .collect(),
            ),
        );
    }
    m.insert("additionalProperties".into(), Value::Bool(false));
    m
}
