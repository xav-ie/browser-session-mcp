//! rmcp `ServerHandler` impl. Defines the tool surface and dispatches each
//! tool call to the right module.
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchDragEventParams, DispatchDragEventType, DispatchKeyEventParams, DispatchKeyEventType,
    DispatchMouseEventParams, DispatchMouseEventType, EventDragIntercepted, InsertTextParams,
    MouseButton, SetInterceptDragsParams,
};
use chromiumoxide::cdp::browser_protocol::{
    network::CookieParam,
    page::{
        CaptureScreenshotFormat, CreateIsolatedWorldParams, EventJavascriptDialogOpening,
        HandleJavaScriptDialogParams, NavigateParams,
    },
    storage::{GetCookiesParams, SetCookiesParams},
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::keys::USKEYBOARD_LAYOUT;
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::chrome_ctx::ChromeContext;
use crate::emulation::{self, Emulation, InitScript, NetworkConditions};
use crate::jev;
use crate::logs::{self, LogKind, ReadOpts, SessionLogEntry};
use crate::profiler;
use crate::saved_states::{SavedStateStore, StateSummary};
use crate::sessions::{SessionInfo, Viewport};
use crate::snapshot;
use crate::state::SessionRecord;
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
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.protocol_version = ProtocolVersion::V_2025_06_18;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("browser-session-mcp", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Per-call isolated browser sessions against a shared persistent Chrome. Call open_browser_session to obtain a sessionId; pass it into every subsequent tool call. For human takeover: show the request_human_takeover URL to the user right away, then call await_human_takeover again and again in a background task until it completes; with no background task, end your turn and call it once the user says they're done.".into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: with_uploads_dir(tool_defs(), crate::uploads_dir().as_deref()),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        let name = request.name.as_ref();
        // While a person has control through a takeover link, the agent keeps
        // its hands off the session: nothing it does lands on the page under
        // them, and nothing reads what they type.
        if let Some(id) = args.get("sessionId").and_then(Value::as_str) {
            if !UNLOCKED_TOOLS.contains(&name) {
                if let Some(token) = takeover::controlling_token(id).await {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "A person has control of this session through a takeover link, so calls on it \
                         are paused until they hand control back. Wait with \
                         await_human_takeover(token=\"{token}\")."
                    ))])
                    .into());
                }
            }
        }
        // One call drives a session at a time, across processes: two agents
        // (or two calls of one) on the same page fight over it.
        let _lock = match args.get("sessionId").and_then(Value::as_str) {
            Some(id) if !UNLOCKED_TOOLS.contains(&name) => match SessionLock::acquire(id, name)
                .await
            {
                Ok(lock) => Some(lock),
                Err(err) => {
                    return Ok(
                        CallToolResult::error(vec![ContentBlock::text(format!("{err:#}"))]).into(),
                    );
                }
            },
            _ => None,
        };
        match crate::frames::scope(self.dispatch(name, args)).await {
            Ok(result) => Ok(result.into()),
            Err(err) => {
                // A transport-level failure may mean the connection died:
                // have the next call probe it (and reconnect only if the
                // probe fails) rather than trust the recent-success window.
                if is_transport_error(&err) {
                    self.ctx.suspect().await;
                }
                Ok(CallToolResult::error(vec![ContentBlock::text(format!("{err:#}"))]).into())
            }
        }
    }
}

/// Tools that only read logs or settings, so they never wait on a session's
/// lock.
const UNLOCKED_TOOLS: &[&str] = &[
    "list_visits",
    "list_console_messages",
    "list_network_requests",
    "get_stealth",
    "get_emulation",
    "list_browser_sessions",
];

/// How long a call waits for another call on its session to finish.
const LOCK_WAIT_MS: u64 = 15_000;

/// A session's lock: an exclusive lock on `$STATE_DIR/locks/<id>.lock`,
/// released when dropped. The file says which call holds it.
struct SessionLock {
    _file: std::fs::File,
}

impl SessionLock {
    async fn acquire(session_id: &str, tool: &str) -> Result<Self> {
        let dir = crate::state_dir().join("locks");
        let path = logs::session_dir(&dir, session_id)?.with_extension("lock");
        let (session_id, tool) = (session_id.to_string(), tool.to_string());
        tokio::task::spawn_blocking(move || -> Result<Self> {
            use std::io::{Seek, Write};
            std::fs::create_dir_all(&dir).context("creating the locks folder")?;
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .context("opening the session lock")?;
            let deadline = std::time::Instant::now() + Duration::from_millis(LOCK_WAIT_MS);
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(std::fs::TryLockError::WouldBlock) => {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        let (held_tool, since) = holder.split_once(' ').unwrap_or(("another call", "0"));
                        let secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs())
                            .saturating_sub(since.trim().parse().unwrap_or(0));
                        bail!(
                            "session {session_id} is in use by another call ({held_tool}, running for {secs}s); \
                             one call drives a session at a time, so wait for it or use another session"
                        );
                    }
                    Err(std::fs::TryLockError::Error(e)) => return Err(e).context("locking the session"),
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            file.set_len(0).ok();
            file.rewind().ok();
            write!(file, "{tool} {now}").ok();
            Ok(Self { _file: file })
        })
        .await
        .context("waiting for the session lock")?
    }
}

impl BrowserSessionServer {
    // --- lifecycle ---

    async fn open_session(&self, args: JsonObject) -> Result<Structured<SessionInfo>> {
        let sessions = self.ctx.sessions().await?;
        let device = match optional_str(&args, "device") {
            Some(d) => Some(crate::devices::find(d).ok_or_else(|| {
                anyhow!(
                    "unknown device `{d}` (use one of {})",
                    crate::devices::names()
                )
            })?),
            None => None,
        };
        // A preset sets the screen, pixel ratio, User-Agent and touch;
        // explicit `viewport`, `useMobileUA` and `userAgent` override it.
        let viewport = optional_viewport(&args, "viewport")?.or(device.map(|d| Viewport {
            width: d.width,
            height: d.height,
            scale: d.scale,
        }));
        let use_mobile =
            optional_bool(&args, "useMobileUA").unwrap_or(device.is_some_and(|d| d.mobile));
        let name = optional_str(&args, "name").map(str::to_string);
        // A named session is reused while it lives, and otherwise starts
        // with the login last saved under its name, so a login outlives
        // the task, the session and a Chrome restart.
        if let Some(name) = &name {
            if let Some(id) = sessions.find_named(name).await? {
                let info = sessions
                    .list()
                    .await?
                    .into_iter()
                    .find(|s| s.session_id == id)
                    .ok_or_else(|| anyhow!("session {id} closed while reopening it"))?;
                return Ok(ok_text_struct(
                    format!(
                        "Reusing session {id}, already open as \"{name}\" (on {}); its viewport and phone setting stay as they were.",
                        info.active_url.as_deref().unwrap_or("a blank tab")
                    ),
                    info,
                ));
            }
        }
        let custom_ua = optional_str(&args, "userAgent").or(device.and_then(|d| d.user_agent));
        let info = sessions.open(viewport, use_mobile, custom_ua).await?;
        let Some(name) = name else {
            return Ok(ok_text_struct(
                format!("Opened session {}", info.session_id),
                info,
            ));
        };
        sessions.set_name(&info.session_id, &name).await;
        let restored = match self.load_login(&info.session_id, &name).await {
            Ok(state) => format!(
                ", with the login saved as \"{name}\" ({} cookies)",
                state.cookies.len()
            ),
            Err(_) => String::new(),
        };
        Ok(ok_text_struct(
            format!("Opened session {} as \"{name}\"{restored}", info.session_id),
            info,
        ))
    }

    async fn close_session(&self, args: JsonObject) -> Result<Text> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Best-effort — closing an unknown sessionId is not an error.
        if let Ok(sm) = self.ctx.sessions().await {
            // A named session keeps its login for the next open.
            if let Some(name) = sm.name(&session_id).await {
                let _ = self.save_login(&session_id, &name).await;
            }
            let _ = sm.close(&session_id).await;
        }
        let writer = logs::LogWriter::new(self.logs_dir.clone());
        let _ = writer.close_session(&session_id).await;
        if let Ok(dir) = logs::session_dir(&crate::state_dir().join("locks"), &session_id) {
            let _ = tokio::fs::remove_file(dir.with_extension("lock")).await;
        }
        Ok(ok_text(format!("Closed session {session_id}")))
    }

    async fn list_sessions(&self, _args: JsonObject) -> Result<Structured<SessionsOut>> {
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
        Ok(ok_text_struct(body, SessionsOut { sessions: list }))
    }

    // --- tabs ---

    async fn new_page(&self, args: JsonObject) -> Result<Structured<TabOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let url = optional_str(&args, "url").map(str::to_string);
        let page = sessions.new_page(&session_id, url.as_deref()).await?;
        let pages = sessions.pages(&session_id).await?;
        let index = pages.len().saturating_sub(1);
        let page_url = page.url().await?.unwrap_or_default();
        Ok(ok_text_struct(
            format!("Opened tab #{index}"),
            TabOut {
                index,
                url: page_url,
            },
        ))
    }

    async fn list_pages(&self, args: JsonObject) -> Result<Structured<PagesOut>> {
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
        Ok(ok_text_struct(body, PagesOut { pages: summaries }))
    }

    async fn switch_tab(&self, args: JsonObject) -> Result<Structured<TabOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let index = optional_u64(&args, "tabIndex")
            .ok_or_else(|| anyhow!("`tabIndex` (integer) is required"))?
            as usize;
        let url = sessions.switch_tab(&session_id, index).await?;
        Ok(ok_text_struct(
            format!("Switched to tab #{index} ({url})"),
            TabOut { index, url },
        ))
    }

    async fn close_tab(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let index = optional_u64(&args, "tabIndex")
            .ok_or_else(|| anyhow!("`tabIndex` (integer) is required"))?
            as usize;
        sessions.close_tab(&session_id, index).await?;
        Ok(ok_text(format!("Closed tab #{index}")))
    }

    async fn navigate(&self, args: JsonObject) -> Result<Structured<NavigateOut>> {
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
        let until = optional_str(&args, "waitUntil")
            .unwrap_or("load")
            .to_string();
        if !matches!(until.as_str(), "load" | "networkidle") {
            bail!("unknown waitUntil `{until}` (use load or networkidle)");
        }
        // Watched from before the navigation, so requests the page starts
        // while loading are counted.
        let inflight = if until == "networkidle" {
            Some(InFlight::watch(&page).await?)
        } else {
            None
        };
        let goto_fut = page.goto(nav);
        if let Some(ms) = timeout_ms {
            tokio::time::timeout(Duration::from_millis(ms), goto_fut)
                .await
                .map_err(|_| anyhow!("navigation timed out after {ms}ms"))?
                .context("Page.navigate")?;
        } else {
            goto_fut.await.context("Page.navigate")?;
        }
        // After the load event, a page that renders itself can still be
        // blank: optionally wait for its requests to go quiet, or for text.
        let settle_ms = optional_u64(&args, "settleTimeout")
            .unwrap_or(10_000)
            .min(MAX_PAUSE_MS);
        let mut note = String::new();
        if let Some(inflight) = &inflight {
            if !inflight.idle(settle_ms).await {
                note = format!(
                    " (requests were still going after {settle_ms}ms; the page may poll continuously)"
                );
            }
        }
        if let Some(text) = optional_str(&args, "text") {
            let body = format!(
                "function() {{ return !!(document.body && document.body.innerText.includes({})); }}",
                serde_json::to_string(text).unwrap_or_default()
            );
            wait_until(
                || async {
                    Ok(iso_eval_function(&page, &body)
                        .await?
                        .as_bool()
                        .unwrap_or(false))
                },
                settle_ms,
            )
            .await
            .with_context(|| {
                format!("the page loaded, but \"{text}\" didn't appear within {settle_ms}ms")
            })?;
        }
        let resolved_url = page.url().await?.unwrap_or(url);
        for d in sessions.take_dialogs(&session_id) {
            note.push_str(&format!(" The page asked ({d}) and it was accepted."));
        }
        Ok(ok_text_struct(
            format!("Navigated to {resolved_url}{note}"),
            NavigateOut { url: resolved_url },
        ))
    }

    // --- capture ---

    async fn take_screenshot(&self, args: JsonObject) -> Result<Screenshot> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let full_page = optional_bool(&args, "fullPage").unwrap_or(false);
        let save = optional_bool(&args, "save").unwrap_or(false);
        let scale = optional_f64(&args, "scale").unwrap_or(1.0);
        if !(0.05..=4.0).contains(&scale) {
            bail!("`scale` must be between 0.05 and 4");
        }
        let page = sessions.active_page(&session_id).await?;
        // One element (scrolled into view), or the screen; `scale` resizes
        // either. Clip coordinates are the document's, not the screen's.
        let clip = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, _) = resolve_and_wait(&page, sel, timeout).await?;
            let (x, y, w, h) = element_box(&page, &css)
                .await?
                .ok_or_else(|| anyhow!("`{sel}` is not visible, so there is nothing to capture"))?;
            Some((x, y, w, h))
        } else if scale != 1.0 && !full_page {
            let vp = sessions.viewport(&session_id).await;
            let scroll = iso_eval_top(&page, "function() { return [scrollX, scrollY]; }").await?;
            let at = |i: usize| scroll.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            Some((at(0), at(1), vp.width as f64, vp.height as f64))
        } else {
            None
        };
        let mut params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .full_page(full_page && clip.is_none());
        if let Some((x, y, width, height)) = clip {
            params = params.clip(chromiumoxide::cdp::browser_protocol::page::Viewport {
                x,
                y,
                width,
                height,
                scale,
            });
        }
        let bytes = page
            .screenshot(params.build())
            .await
            .context("Page.captureScreenshot")?;
        let len = bytes.len();
        if save {
            // Saved under the state folder, which the host shares, so a
            // caller that can't take an image (a sandbox) gets a path; not
            // with the session's logs, which closing it deletes.
            let dir = logs::session_dir(&crate::state_dir().join("screenshots"), &session_id)?;
            tokio::fs::create_dir_all(&dir)
                .await
                .context("creating the screenshots folder")?;
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let path = dir.join(format!("{ms}.png"));
            tokio::fs::write(&path, &bytes)
                .await
                .context("saving the screenshot")?;
            return Ok(Screenshot {
                text: format!("Saved {len} bytes PNG to {}", path.display()),
                png_base64: None,
                saved: Some((path, len)),
            });
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(Screenshot {
            text: format!("Captured {len} bytes PNG."),
            png_base64: Some(b64),
            saved: None,
        })
    }

    async fn take_snapshot(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let tree = snapshot::snapshot(&page).await?;
        Ok(ok_text(tree))
    }

    // --- interact ---

    async fn click(&self, args: JsonObject) -> Result<Structured<AfterAction>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        // CDP Input (trusted events): reliably fires handlers AND grants user
        // activation, so it can open popups/new tabs that a synthetic JS
        // `.click()` cannot. Target a selector's center or explicit x/y.
        let (x, y, label, target) = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, note) = resolve_and_wait(&page, sel, timeout).await?;
            let (x, y) = element_center(&page, &css).await?;
            (x, y, note.unwrap_or(css.clone()), Some(css))
        } else if let (Some(x), Some(y)) = (optional_f64(&args, "x"), optional_f64(&args, "y")) {
            (x, y, format!("({x}, {y})"), None)
        } else {
            bail!("click requires either `selector` or both `x` and `y`");
        };
        let before = before_action(&page).await;
        let hit = check_point(&page, (x, y), target.as_deref(), &label).await?;
        page.click(Point::new(x, y))
            .await
            .with_context(|| format!("clicking {label}"))?;
        let after = after_action(&page, before, Some(hit), (&sessions, &session_id)).await;
        Ok(ok_text_struct(
            after.render(&format!("Clicked {label}")),
            after,
        ))
    }

    async fn press_key(&self, args: JsonObject) -> Result<Structured<AfterAction>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let key = required_str(&args, "key")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let before = before_action(&page).await;
        // None unless Jev picked the element; names what was chosen.
        let mut note = None;
        if let Some(sel) = optional_str(&args, "selector") {
            // Focus the element, then press (e.g. type into a field then Enter).
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, resolved) = resolve_and_wait(&page, sel, timeout).await?;
            note = resolved;
            focus_element(&page, &css)
                .await
                .with_context(|| format!("locating {sel}"))?;
        }
        // Repeated presses (a Tab walk) report focus after each, in one call
        // instead of one call per press.
        let repeat = optional_u64(&args, "repeat")
            .unwrap_or(1)
            .clamp(1, MAX_KEY_REPEAT);
        let mut trail = Vec::new();
        for i in 0..repeat {
            dispatch_key(&page, &key)
                .await
                .with_context(|| format!("pressing {key}"))?;
            if repeat > 1 && i + 1 < repeat {
                tokio::time::sleep(Duration::from_millis(30)).await;
                trail.push(focused_now(&page).await);
            }
        }
        let on = note.map(|n| format!(" on {n}")).unwrap_or_default();
        let mut after = after_action(&page, before, None, (&sessions, &session_id)).await;
        let times = if repeat > 1 {
            format!(" {repeat} times")
        } else {
            String::new()
        };
        if repeat > 1 {
            trail.push(after.focused.clone());
            after.focus_trail = Some(trail);
        }
        Ok(ok_text_struct(
            after.render(&format!("Pressed {key}{on}{times}")),
            after,
        ))
    }

    async fn scroll(&self, args: JsonObject) -> Result<Text> {
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

    async fn tap(&self, args: JsonObject) -> Result<Structured<AfterAction>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let (at, label, target) = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, note) = resolve_and_wait(&page, sel, timeout).await?;
            (
                element_center(&page, &css).await?,
                note.unwrap_or(css.clone()),
                Some(css),
            )
        } else if let (Some(x), Some(y)) = (optional_f64(&args, "x"), optional_f64(&args, "y")) {
            ((x, y), format!("({x}, {y})"), None)
        } else {
            bail!("tap requires either `selector` or both `x` and `y`");
        };
        let before = before_action(&page).await;
        let hit = check_point(&page, at, target.as_deref(), &label).await?;
        touch_tap(&page, at)
            .await
            .with_context(|| format!("tapping {label}"))?;
        let after = after_action(&page, before, Some(hit), (&sessions, &session_id)).await;
        Ok(ok_text_struct(
            after.render(&format!("Tapped {label}")),
            after,
        ))
    }

    async fn swipe(&self, args: JsonObject) -> Result<Structured<AfterAction>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let vp = sessions.viewport(&session_id).await;
        let (from, label) = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, note) = resolve_and_wait(&page, sel, timeout).await?;
            (element_center(&page, &css).await?, note.unwrap_or(css))
        } else if let (Some(x), Some(y)) = (optional_f64(&args, "x"), optional_f64(&args, "y")) {
            ((x, y), format!("({x}, {y})"))
        } else {
            let mid = (vp.width as f64 / 2.0, vp.height as f64 / 2.0);
            (mid, "the middle of the screen".to_string())
        };
        let to = if let (Some(x), Some(y)) =
            (optional_f64(&args, "toX"), optional_f64(&args, "toY"))
        {
            (x, y)
        } else {
            let direction = optional_str(&args, "direction").ok_or_else(|| {
                anyhow!("swipe needs a `direction` (left, right, up, down) or both `toX` and `toY`")
            })?;
            let along_x = matches!(direction, "left" | "right");
            let default = 0.6 * if along_x { vp.width } else { vp.height } as f64;
            let d = optional_f64(&args, "distance").unwrap_or(default);
            match direction {
                "left" => (from.0 - d, from.1),
                "right" => (from.0 + d, from.1),
                "up" => (from.0, from.1 - d),
                "down" => (from.0, from.1 + d),
                other => bail!("unknown direction `{other}` (use left, right, up or down)"),
            }
        };
        let duration = optional_u64(&args, "durationMs").unwrap_or(300);
        let before = before_action(&page).await;
        touch_swipe(&page, from, to, duration)
            .await
            .with_context(|| format!("swiping from {label}"))?;
        let after = after_action(&page, before, None, (&sessions, &session_id)).await;
        Ok(ok_text_struct(
            after.render(&format!(
                "Swiped from {label} to ({:.0}, {:.0}) over {duration}ms",
                to.0, to.1
            )),
            after,
        ))
    }

    async fn move_mouse(&self, args: JsonObject) -> Result<Text> {
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

    async fn type_text(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = required_str(&args, "selector")?.to_string();
        let text = required_str(&args, "text")?.to_string();
        let clear = optional_bool(&args, "clear").unwrap_or(false);
        let delay = optional_u64(&args, "delay").unwrap_or(0);
        let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
        let page = sessions.active_page(&session_id).await?;
        let (selector, note) = resolve_and_wait(&page, &selector, timeout).await?;
        let typed = type_into(&page, &selector, &text, clear, delay).await?;
        let target = note.unwrap_or(selector);
        Ok(ok_text(typed_report(&typed, &text, &target, clear)?))
    }

    async fn select_option(&self, args: JsonObject) -> Result<Structured<SelectOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = required_str(&args, "selector")?.to_string();
        let want = optional_str(&args, "option").map(str::to_string);
        let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
        let page = sessions.active_page(&session_id).await?;
        let (css, note) = resolve_and_wait_for(
            &page,
            &selector,
            timeout,
            Some("a dropdown or a set of options to choose one from"),
            true,
        )
        .await?;
        let label = note.unwrap_or(css.clone());
        let before = before_action(&page).await;
        let choice = choose_option(&page, &css, &label, want.as_deref()).await?;
        let mut after = after_action(&page, before, None, (&sessions, &session_id)).await;
        // The options are this result's own field; the report's would repeat them.
        after.options = None;
        let Some(chosen) = choice.chosen else {
            let text = format!(
                "{label} offers {} option{}: {}. Nothing was chosen.",
                choice.options.len(),
                plural(choice.options.len()),
                option_list(&choice.options)
            );
            return Ok(ok_text_struct(
                text,
                SelectOut {
                    chosen: None,
                    shown: None,
                    confirmed: None,
                    options: choice.options,
                    after,
                },
            ));
        };
        let (shown, confirmed) = shown_value(&page, &css, &chosen).await;
        let mut text = after.render(&format!("Chose {chosen:?} in {label}"));
        if confirmed {
            text.push_str(&format!(" The field shows {shown:?}."));
        } else {
            text.push_str(&format!(
                " The field doesn't show it in full (it shows {shown:?}); a field can show a short form, so check with take_screenshot or read if it matters."
            ));
        }
        Ok(ok_text_struct(
            text,
            SelectOut {
                chosen: Some(chosen),
                shown: Some(shown),
                confirmed: Some(confirmed),
                options: choice.options,
                after,
            },
        ))
    }

    async fn upload_file(&self, args: JsonObject) -> Result<Structured<UploadOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = required_str(&args, "selector")?.to_string();
        let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
        let (bytes, name) = match (optional_str(&args, "path"), optional_str(&args, "content")) {
            (Some(path), None) => {
                let bytes = std::fs::read(path).with_context(|| {
                    match crate::uploads_dir() {
                        Some(d) => format!("reading {path} on the server; save the file into {}/ and pass that path, or send it as `content` (base64)", d.display()),
                        None => format!("reading {path} on the server; a file on another machine goes in `content` as base64"),
                    }
                })?;
                let name = optional_str(&args, "name")
                    .map(str::to_string)
                    .or_else(|| {
                        std::path::Path::new(path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "file".to_string());
                (bytes, name)
            }
            (None, Some(content)) => {
                let name = optional_str(&args, "name")
                    .ok_or_else(|| anyhow!("`content` needs a file `name`, such as resume.pdf"))?;
                let clean: String = content.split_whitespace().collect();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(clean.as_bytes())
                    .context("`content` is not valid base64")?;
                (bytes, name.to_string())
            }
            _ => bail!("upload_file takes either `path` or `content` (base64) with `name`"),
        };
        if bytes.len() > MAX_UPLOAD_BYTES {
            bail!(
                "the file is {} bytes; upload_file takes up to {MAX_UPLOAD_BYTES}",
                bytes.len()
            );
        }
        let mime = optional_str(&args, "mimeType")
            .unwrap_or_else(|| mime_for(&name))
            .to_string();
        let page = sessions.active_page(&session_id).await?;
        let (css, note) = resolve_and_wait_for(
            &page,
            &selector,
            timeout,
            Some("a file upload field or drop zone"),
            false,
        )
        .await?;
        let label = note.unwrap_or(css.clone());
        let before = before_action(&page).await;
        let decl = UPLOAD_JS
            .replace("@@DEEP@@", jev::DEEP_FIND_JS)
            .replace("@@DESC@@", DESCRIBE_JS)
            .replace("@@SEL@@", &js_str(&css))
            .replace("@@NAME@@", &js_str(&name))
            .replace("@@TYPE@@", &js_str(&mime))
            .replace(
                "@@B64@@",
                &js_str(&base64::engine::general_purpose::STANDARD.encode(&bytes)),
            );
        let v = iso_eval_function(&page, &decl).await?;
        if v.is_null() {
            bail!("element `{css}` not found");
        }
        let how = v
            .get("how")
            .and_then(Value::as_str)
            .unwrap_or("input")
            .to_string();
        let into = v
            .get("into")
            .and_then(Value::as_str)
            .unwrap_or("the page")
            .to_string();
        let accept = v.get("accept").and_then(Value::as_str).unwrap_or_default();
        let after = after_action(&page, before, None, (&sessions, &session_id)).await;
        // Pages often upload the file before listing it, and may show the
        // name in capitals, so look for a while and ignore case.
        let shows = format!(
            "function() {{ return String(document.body && document.body.innerText || '').toLowerCase().indexOf({}) >= 0; }}",
            js_str(&name.to_lowercase())
        );
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2_000);
        let page_shows_name = loop {
            let seen = iso_eval_function(&page, &shows)
                .await
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if seen || tokio::time::Instant::now() >= deadline {
                break seen;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        };
        let mut text = after.render(&format!(
            "{} {name} ({} bytes, {mime}) {} {into} (for {label})",
            if how == "drop" { "Dropped" } else { "Attached" },
            bytes.len(),
            if how == "drop" { "onto" } else { "to" },
        ));
        if !accepts(accept, &name, &mime) {
            text.push_str(&format!(
                " The input only accepts {accept}, so the page may reject it."
            ));
        }
        text.push_str(if page_shows_name {
            " The page shows the file name."
        } else {
            " The page doesn't show the file name; check with take_screenshot that it took the file."
        });
        Ok(ok_text_struct(
            text,
            UploadOut {
                name,
                bytes: bytes.len(),
                mime_type: mime,
                how,
                into,
                page_shows_name,
                after,
            },
        ))
    }

    async fn ask(&self, args: JsonObject) -> Result<Structured<AskOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let question = required_str(&args, "question")?.to_string();
        let options: Vec<String> = match args.get("options") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|o| {
                    o.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("`options` must be strings"))
                })
                .collect::<Result<_>>()?,
            Some(_) => bail!("`options` must be an array of strings"),
        };
        if options.len() == 1 || options.len() > 255 {
            bail!("`options` needs 2 to 255 entries, or leave it out for a yes/no question");
        }
        let page = sessions.active_page(&session_id).await?;
        let a = jev::ask(&page, &question, &options).await?;
        let text = if options.is_empty() {
            format!("{} (probability of yes {:.2})", a.answer, a.probability)
        } else {
            format!("{} (confidence {:.2})", a.answer, a.probability)
        };
        Ok(ok_text_struct(
            text,
            AskOut {
                answer: a.answer,
                probability: a.probability,
                probabilities: a
                    .probabilities
                    .into_iter()
                    .map(|(option, probability)| AskOption {
                        option,
                        probability,
                    })
                    .collect(),
            },
        ))
    }

    async fn read(&self, args: JsonObject) -> Result<Structured<ReadOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let question = required_str(&args, "question")?.to_string();
        let max_chars = optional_u64(&args, "maxChars").unwrap_or(12_000).max(500) as usize;
        let page = sessions.active_page(&session_id).await?;
        let r = jev::read(&page, &question, max_chars).await?;
        let mut text = String::new();
        let mut heading = None;
        for p in &r.passages {
            if heading != Some(&p.heading) {
                if !p.heading.is_empty() {
                    text.push_str(&format!("## {}\n\n", p.heading));
                }
                heading = Some(&p.heading);
            }
            text.push_str(&p.text);
            text.push_str("\n\n");
        }
        let text = text.trim_end().to_string();
        let summary = if !r.filtered {
            format!(
                "The page is short ({} characters), so here it is whole.",
                r.page_chars
            )
        } else if r.passages.is_empty() {
            format!(
                "Nothing among the page's {} passages ({} characters) looked relevant to the question.",
                r.total, r.page_chars
            )
        } else {
            format!(
                "{} of the page's {} passages looked relevant ({} of {} characters).",
                r.passages.len(),
                r.total,
                text.chars().count(),
                r.page_chars
            )
        };
        Ok(ok_text_struct(
            if text.is_empty() {
                summary.clone()
            } else {
                format!("{summary}\n\n{text}")
            },
            ReadOut {
                text,
                url: r.url,
                title: r.title,
                page_chars: r.page_chars,
                passages: r.total,
                kept: r.passages.len(),
                filtered: r.filtered,
            },
        ))
    }

    async fn act(&self, args: JsonObject) -> Result<Structured<ActOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let goal = required_str(&args, "goal")?.to_string();
        let max_steps = optional_u64(&args, "maxSteps").unwrap_or(8).clamp(1, 32);
        let mut values: Vec<(String, String)> = Vec::new();
        if let Some(v) = args.get("values") {
            let map = v.as_object().ok_or_else(|| {
                anyhow!("`values` must be an object like {{ query: \"Oolong\" }}")
            })?;
            for (k, val) in map {
                let text = val
                    .as_str()
                    .ok_or_else(|| anyhow!("values.{k} must be a string"))?
                    .to_string();
                values.push((k.clone(), text));
            }
        }
        let keys: Vec<String> = values.iter().map(|(k, _)| k.clone()).collect();
        let page = sessions.active_page(&session_id).await?;
        if !jev::available() {
            bail!(
                "act needs a Jev key: set TYPESAFE_API_KEY on the server (keys: console.typesafe.ai)"
            );
        }
        let confirm_irreversible = optional_bool(&args, "confirmIrreversible").unwrap_or(false);
        // A mobile session gets touch input: taps for clicks, finger drags,
        // and swipes as actions of their own.
        let touch = sessions.is_mobile(&session_id).await;
        let vp = sessions.viewport(&session_id).await;
        let mut page = page;
        let floor = jev::ACT_MIN_CONFIDENCE;
        let mut steps: Vec<ActStep> = Vec::new();
        let mut recent: Vec<String> = Vec::new();
        let mut url = String::new();
        let mut title = String::new();
        let mut no_effect = 0usize;
        let mut empty_reads = 0usize;
        let mut last_element: Option<String> = None;
        let mut last_had_effect = false;
        let mut last_goal_met = 0.0f64;
        let mut typed_keys: Vec<String> = Vec::new();
        clear_typed_marks(&page).await;
        let dialogs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut guards = vec![DialogGuard::watch(&page, dialogs.clone()).await?];
        let extra = |candidates: Vec<ActCandidate>, pending: Option<String>| ActExtra {
            candidates,
            pending,
            dialogs: dialogs.lock().map(|d| d.clone()).unwrap_or_default(),
        };
        for step in 1..=max_steps {
            let untyped: Vec<String> = keys
                .iter()
                .filter(|k| !typed_keys.contains(k))
                .cloned()
                .collect();
            // The goal-met level this step would stop at, so a big page's
            // group request can end the run without a second request.
            let just_typed = steps.last().is_some_and(|s| s.action == "type");
            let stop_at = if !steps.is_empty() && !just_typed {
                ACT_LIKELY_GATE
            } else {
                ACT_GATE
            };
            let d = match jev::decide_step(&page, &goal, &recent, &untyped, stop_at, touch).await {
                Ok(d) => d,
                Err(e) => bail!("act failed at step {step}: {e:#}; {}", trail(&steps)),
            };
            url = d.page.url.clone();
            title = d.page.title.clone();
            last_goal_met = d.goal_met;
            // Gates run before executing anything this step: if the goal
            // already reads met, or nothing can help, don't act at all.
            if d.goal_met >= ACT_GATE {
                return Ok(act_done(
                    "completed",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            // Once something was done, a goal that reads probably met is a
            // stop: Jev rarely calls an achieved goal certain, and acting on
            // past it (a further click, going back) tends to undo it. Not
            // right after typing, though: a typed search shows its result as
            // a suggestion before it is opened, and read as done. A goal
            // that typing does finish still ends likely_completed at the
            // next stop.
            if !steps.is_empty() && !just_typed && d.goal_met >= ACT_LIKELY_GATE {
                return Ok(act_done(
                    "likely_completed",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            if d.bot_check >= ACT_WALL_GATE {
                return Ok(act_done(
                    "needs_human",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            if d.stuck >= ACT_GATE {
                let status = if d.goal_met >= ACT_LIKELY_GATE {
                    "likely_completed"
                } else {
                    "stuck"
                };
                return Ok(act_done(
                    status,
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            // With `values` passed, a login form may be the work itself
            // (the caller supplied what to type), so only a value-less run
            // treats a sign-in wall as a stop.
            if d.login >= ACT_WALL_GATE && values.is_empty() {
                return Ok(act_done(
                    "needs_login",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            if d.error >= ACT_WALL_GATE {
                return Ok(act_done(
                    "blocked",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            // A page with no elements is usually mid-navigation: give it a
            // moment before calling it empty, unless Jev picked an action
            // that needs no element (a swipe on a carousel of plain slides,
            // a scroll, a key).
            if !d.has_elements && (d.chosen == "wait" || d.confidence < floor) {
                empty_reads += 1;
                if empty_reads <= ACT_EMPTY_RETRIES {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                return Ok(act_done(
                    "stuck",
                    &goal,
                    steps,
                    url,
                    title,
                    extra(vec![], None),
                ));
            }
            empty_reads = 0;
            if d.confidence < floor {
                // Not sure enough to act: hand the decision back with what
                // Jev was weighing instead of guessing or erroring.
                let candidates = d
                    .candidates
                    .iter()
                    .map(|(id, p)| ActCandidate {
                        target: d.describe(id),
                        probability: *p,
                    })
                    .collect();
                let status = if d.goal_met >= ACT_LIKELY_GATE {
                    "likely_completed"
                } else {
                    "uncertain"
                };
                return Ok(act_done(
                    status,
                    &goal,
                    steps,
                    url,
                    title,
                    extra(candidates, None),
                ));
            }
            // No-effect recovery: if Jev re-picks the element that just did
            // nothing, switch to the next-best element from the same
            // answer's probability ranking (jev-browser's recovery move).
            let mut chosen = d.chosen.clone();
            let mut switched = false;
            if last_element.as_deref() == Some(d.chosen.as_str()) && !last_had_effect {
                if let Some((alt, _)) = &d.next_best {
                    if alt != &d.chosen {
                        chosen = alt.clone();
                        switched = true;
                    }
                }
            }
            let el = if d.is_element {
                d.page.elements.iter().find(|e| e.id == chosen)
            } else {
                None
            };
            if let Some(el) = el {
                if confirm_irreversible
                    && d.irreversible >= ACT_IRREVERSIBLE_GATE
                    && !jev::is_text_field(&el.desc)
                {
                    return Ok(act_done(
                        "needs_confirmation",
                        &goal,
                        steps,
                        url,
                        title,
                        extra(vec![], Some(el.desc.clone())),
                    ));
                }
            }
            // Work in the chosen element's frame, when it's in one.
            if let Some(el) = el {
                let _ = wait_for_selector(&page, &d.page.css(&el.id), 0).await;
            }
            // Snapshot what the action could change, for the effect check,
            // and the open tabs, to follow a link that opens a new one.
            let before = page_fingerprint(&page).await;
            let dialogs_before = dialogs.lock().map(|d| d.len()).unwrap_or(0);
            let tabs_before: Vec<String> = sessions
                .tabs(&session_id)
                .await
                .map(|(t, _)| t.into_iter().map(|t| t.target_id.inner().clone()).collect())
                .unwrap_or_default();
            let (record, recent_line) = if let Some(el) = el {
                let css = d.page.css(&el.id);
                let attempt: Result<(String, Option<String>, Option<String>)> = async {
                    if !el.options.is_empty() {
                        return match jev::decide_select_option(&goal, &el.desc, &el.options).await {
                            Ok(Some((i, label, _))) => {
                                set_select_option(&page, &css, i, &label).await?;
                                Ok(("select".to_string(), None, None))
                            }
                            Ok(None) => {
                                click_label(&page, &css, &el.desc).await?;
                                Ok(("click".to_string(), None, None))
                            }
                            Err(e) => bail!(
                                "act failed choosing an option at step {step}: {e:#}; {}",
                                trail(&steps)
                            ),
                        };
                    }
                    if jev::is_text_field(&el.desc) {
                        // The step's value guess was made before the field
                        // was known; when it found nothing, ask again about
                        // this field. A field no value fits just gets clicked.
                        let mut key = d
                            .field_values
                            .get(&el.id)
                            .filter(|k| untyped.contains(k))
                            .or(d.value_key.as_ref().filter(|k| untyped.contains(k)))
                            .cloned();
                        if key.is_none() && !untyped.is_empty() {
                            key = jev::decide_value_for_field(&goal, &el.desc, &untyped)
                                .await
                                .with_context(|| {
                                    format!(
                                        "act failed choosing a value at step {step}; {}",
                                        trail(&steps)
                                    )
                                })?;
                        }
                        if let Some((k, text)) =
                            key.and_then(|k| values.iter().find(|(vk, _)| *vk == k))
                        {
                            type_into(&page, &css, text, true, 0).await?;
                            mark_typed(&page, &css).await;
                            return Ok(("type".to_string(), Some(k.clone()), None));
                        }
                    }
                    match d.operation.as_str() {
                        "hover" => {
                            let (x, y) = element_center(&page, &css).await?;
                            page.move_mouse(Point::new(x, y))
                                .await
                                .context("Input.dispatchMouseEvent (move)")?;
                            return Ok(("hover".to_string(), None, None));
                        }
                        "right_click" => {
                            let at = element_center(&page, &css).await?;
                            right_click(&page, at).await?;
                            return Ok(("right_click".to_string(), None, None));
                        }
                        "drag" => {
                            let target = jev::decide_drop_target(&goal, &d.page, &el.id)
                                .await
                                .with_context(|| {
                                    format!(
                                        "act failed choosing where to drop at step {step}; {}",
                                        trail(&steps)
                                    )
                                })?;
                            if let Some(target) = target {
                                let from = element_center(&page, &css).await?;
                                let to = element_center(&page, &d.page.css(&target)).await?;
                                if touch {
                                    touch_swipe(&page, from, to, 400).await?;
                                } else {
                                    drag(&page, from, to).await?;
                                }
                                return Ok(("drag".to_string(), None, Some(d.describe(&target))));
                            }
                        }
                        "swipe" if touch => {
                            let from = element_center(&page, &css).await?;
                            touch_swipe(&page, from, swipe_end(from, &d.direction, vp), 300)
                                .await?;
                            return Ok((format!("swipe_{}", d.direction), None, None));
                        }
                        _ => {}
                    }
                    if touch {
                        let at = element_center(&page, &css).await?;
                        check_point(&page, at, Some(&css), &el.desc).await?;
                        touch_tap(&page, at)
                            .await
                            .with_context(|| format!("tapping {}", el.desc))?;
                        return Ok(("tap".to_string(), None, None));
                    }
                    click_label(&page, &css, &el.desc).await?;
                    Ok(("click".to_string(), None, None))
                }
                .await;
                // A page that re-rendered between the pick and the action
                // has dropped the picked element; decide again on the new
                // page rather than failing the whole run.
                // A target something covers (a popup, a banner) isn't
                // clicked; the next step reads what is in the way.
                let mut blocked = None;
                let (action, typed, dropped_on) = match attempt {
                    Ok(done) => done,
                    Err(e) if format!("{e:#}").contains("not found") => {
                        ("missed".to_string(), None, None)
                    }
                    Err(e) if format!("{e:#}").contains("is covered by") => {
                        blocked = format!("{e:#}")
                            .split(" is covered by ")
                            .nth(1)
                            .and_then(|r| r.split(" at (").next())
                            .map(str::to_string);
                        ("blocked".to_string(), None, None)
                    }
                    Err(e) => return Err(e),
                };
                if let Some(k) = &typed {
                    typed_keys.push(k.clone());
                }
                (
                    ActStep {
                        step: step as usize,
                        action: action.clone(),
                        target: Some(el.desc.clone()),
                        typed: typed.clone(),
                        confidence: d.confidence,
                        goal: d.goal_met,
                        stuck: d.stuck,
                    },
                    // Typing is reported by key and without the field's
                    // "(empty)", which the history would otherwise keep
                    // saying after the field was filled.
                    match &typed {
                        Some(k) => format!(
                            "{step}. typed the value for {k} into {}",
                            el.desc.trim_end_matches(" (empty)")
                        ),
                        None => match (&dropped_on, &blocked) {
                            (Some(t), _) => format!("{step}. dragged {} onto {t}", el.desc),
                            (None, Some(b)) => {
                                format!("{step}. could not click {}: {b} covers it", el.desc)
                            }
                            (None, None) => format!("{step}. {action} {}", el.desc),
                        },
                    },
                )
            } else {
                match d.chosen.as_str() {
                    "scroll_down" => scroll_viewport(&page, 0.8).await?,
                    "scroll_up" => scroll_viewport(&page, -0.8).await?,
                    "back" => go_back(&page).await?,
                    "wait" => tokio::time::sleep(Duration::from_millis(500)).await,
                    "press_key" => dispatch_key(&page, &d.key).await?,
                    swipe if touch && swipe.starts_with("swipe_") => {
                        let direction = swipe.trim_start_matches("swipe_");
                        let from = swipe_start(&page, direction, vp).await;
                        touch_swipe(&page, from, swipe_end(from, direction, vp), 300).await?
                    }
                    other => bail!("Jev chose `{other}`, which is not one of the offered actions"),
                }
                (
                    ActStep {
                        step: step as usize,
                        action: d.chosen.clone(),
                        target: (d.chosen == "press_key").then(|| d.key.clone()),
                        typed: None,
                        confidence: d.confidence,
                        goal: d.goal_met,
                        stuck: d.stuck,
                    },
                    if d.chosen == "press_key" {
                        format!("{step}. pressed {}", d.key)
                    } else {
                        format!("{step}. {}", d.chosen)
                    },
                )
            };
            steps.push(record);
            recent.push(recent_line);
            if switched {
                recent
                    .last_mut()
                    .expect("just pushed")
                    .push_str(" (next-best after the previous action had no effect)");
            }
            // Give the page a beat to react before deciding again: a click
            // that navigates tears the document down mid-flight, and the
            // next extraction would run against a dying DOM (seen live on
            // Wikipedia right after a navigational click). The extraction
            // retries in jev.rs cover slower transitions.
            tokio::time::sleep(Duration::from_millis(ACT_SETTLE_MS)).await;
            // A click that opened a tab moves the work there, as it would
            // for a person.
            if let Ok((tabs, _)) = sessions.tabs(&session_id).await {
                let fresh = tabs
                    .iter()
                    .position(|t| !tabs_before.contains(t.target_id.inner()));
                if let (Some(index), false) = (fresh, tabs_before.is_empty()) {
                    sessions.switch_tab(&session_id, index).await?;
                    page = sessions.active_page(&session_id).await?;
                    guards.push(DialogGuard::watch(&page, dialogs.clone()).await?);
                    recent
                        .last_mut()
                        .expect("just pushed")
                        .push_str(" (opened a new tab; continuing there)");
                    continue;
                }
            }
            // Did that action do anything visible? Compare the page
            // fingerprint (location, title, visible text, dialog, element
            // and form state) from before and after, so modals, toggles and
            // in-place re-renders count, not just navigation. Typing,
            // scrolling, waiting and key presses count as effect by
            // definition; a failed read (mid-navigation teardown) counts as
            // a change.
            let soft_action = matches!(
                steps.last().map(|s| s.action.as_str()),
                Some("type")
                    | Some("scroll_down")
                    | Some("scroll_up")
                    | Some("wait")
                    | Some("press_key")
            );
            let after = page_fingerprint(&page).await;
            // A dialog the action opened is an effect too, and the next
            // decision should know what it said.
            let new_dialogs: Vec<String> = dialogs
                .lock()
                .map(|d| d.iter().skip(dialogs_before).cloned().collect())
                .unwrap_or_default();
            for msg in &new_dialogs {
                recent
                    .last_mut()
                    .expect("just pushed")
                    .push_str(&format!(" (the page showed a dialog: {msg}, accepted)"));
            }
            let had_effect = !new_dialogs.is_empty()
                || soft_action
                || match (&before, &after) {
                    (Some(b), Some(a)) => b != a,
                    _ => true,
                };
            if let Some((u, t)) = current_url_title(&page).await {
                url = u;
                title = t;
            }
            if d.is_element {
                if had_effect {
                    no_effect = 0;
                } else {
                    no_effect += 1;
                    recent
                        .last_mut()
                        .expect("just pushed")
                        .push_str(" (no visible effect)");
                }
                last_element = Some(chosen.clone());
                last_had_effect = had_effect;
                if no_effect >= ACT_NO_EFFECT_LIMIT {
                    let status = if last_goal_met >= ACT_LIKELY_GATE {
                        "likely_completed"
                    } else {
                        "no_progress"
                    };
                    return Ok(act_done(
                        status,
                        &goal,
                        steps,
                        url,
                        title,
                        extra(vec![], None),
                    ));
                }
            }
        }
        drop(guards);
        let status = if last_goal_met >= ACT_LIKELY_GATE {
            "likely_completed"
        } else {
            "max_steps"
        };
        Ok(act_done(
            status,
            &goal,
            steps,
            url,
            title,
            extra(vec![], None),
        ))
    }

    async fn wait_for(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = optional_str(&args, "selector").map(str::to_string);
        let text = optional_str(&args, "text").map(str::to_string);
        let expression = optional_str(&args, "expression").map(str::to_string);
        let timeout = optional_u64(&args, "timeout").unwrap_or(10_000);
        if selector.is_none() && text.is_none() && expression.is_none() {
            // A plain pause: some hosts (Executor's sandbox) have no timer.
            let Some(ms) = optional_u64(&args, "ms") else {
                bail!("wait_for requires `selector`, `text`, `expression` or `ms`.");
            };
            let ms = ms.min(MAX_PAUSE_MS);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            return Ok(ok_text(format!("Waited {ms}ms")));
        }
        if let Some(s) = selector.as_deref().filter(|s| s.starts_with("jev:")) {
            bail!(
                "wait_for doesn't support `jev:` ({s:?}) — wait for `text` content instead, \
                 then click/type with a plain-language or CSS selector"
            );
        }
        let page = sessions.active_page(&session_id).await?;
        if let Some(s) = selector {
            wait_for_selector(&page, &s, timeout).await?;
            return Ok(ok_text(format!("Matched selector {s}")));
        }
        if let Some(expr) = expression {
            // Run like `evaluate` (main world, or isolated on stealth
            // sessions), until the value is truthy.
            let stealth = takeover::is_stealth(&session_id).await;
            let frame = match optional_str(&args, "frame") {
                Some(f) => Some(frame_by_url(&page, f).await?),
                None => None,
            };
            let wrapped = wrap_expression(&page, &expr, stealth || frame.is_some()).await?;
            let last = std::sync::Mutex::new(Value::Null);
            let (last, page, wrapped, frame) = (&last, &page, &wrapped, &frame);
            let found = wait_until(
                || async move {
                    let v = if let Some(frame) = frame {
                        crate::frames::eval(frame, wrapped).await
                    } else if stealth {
                        iso_eval_function(page, wrapped).await
                    } else {
                        main_world_eval(page, wrapped).await
                    }
                    .unwrap_or(Value::Null);
                    let truthy = match &v {
                        Value::Null => false,
                        Value::Bool(b) => *b,
                        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
                        Value::String(s) => !s.is_empty(),
                        _ => true,
                    };
                    *last.lock().expect("unpoisoned") = v;
                    Ok(truthy)
                },
                timeout,
            )
            .await;
            let last = last.lock().expect("unpoisoned").clone();
            found.with_context(|| {
                format!("waiting for `{expr}` to be truthy (last value: {last})")
            })?;
            return Ok(ok_text(format!("`{expr}` became {last}")));
        }
        let needle = text.unwrap();
        let body = format!(
            "function() {{ return document.body && document.body.innerText && document.body.innerText.includes({}); }}",
            serde_json::to_string(&needle).unwrap()
        );
        let page = &page;
        let body = &body;
        wait_until(
            || async move {
                if iso_eval_top(page, body).await?.as_bool().unwrap_or(false) {
                    return Ok(true);
                }
                for frame in crate::frames::closed(page).await {
                    if crate::frames::eval(&frame, body)
                        .await
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            },
            timeout,
        )
        .await?;
        Ok(ok_text(format!("Matched text \"{needle}\"")))
    }

    async fn evaluate(&self, args: JsonObject) -> Result<Structured<EvaluateOut>> {
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
        // Main world by default — so `window.dataLayer`, `__NEXT_DATA__`, and
        // other page-author globals are readable. Only stealth sessions
        // (console-capture toggled OFF → stealth flag present) fall back to an
        // isolated world, which is invisible to page scripts but can't see those
        // globals. The session already exposes Runtime via the listener when not
        // in stealth, so main-world execution costs no extra detectability there.
        let stealth = takeover::is_stealth(&session_id).await;
        let frame = match optional_str(&args, "frame") {
            Some(f) => Some(frame_by_url(&page, f).await?),
            None => None,
        };
        let wrapped = wrap_expression(&page, &expression, stealth || frame.is_some()).await?;
        let value = if let Some(frame) = &frame {
            crate::frames::eval(frame, &wrapped).await
        } else if stealth {
            iso_eval_function(&page, &wrapped).await
        } else {
            main_world_eval(&page, &wrapped).await
        }
        .context("evaluate")?;
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(ok_text_struct(text, EvaluateOut { result: value }))
    }

    async fn measure_page(&self, args: JsonObject) -> Result<Structured<crate::measure::Measure>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        let timeout = optional_u64(&args, "timeout")
            .unwrap_or(20_000)
            .min(MAX_PAUSE_MS);
        // A fresh load, watched from its start so its requests are counted.
        let inflight = InFlight::watch(&page).await?;
        let load = async {
            match optional_str(&args, "url") {
                Some(url) => {
                    let nav = NavigateParams::builder()
                        .url(url.to_string())
                        .build()
                        .map_err(|e| anyhow!("NavigateParams: {e}"))?;
                    page.goto(nav).await.context("Page.navigate").map(|_| ())
                }
                None => page.reload().await.context("Page.reload").map(|_| ()),
            }
        };
        tokio::time::timeout(Duration::from_millis(timeout), load)
            .await
            .map_err(|_| anyhow!("the page didn't finish loading within {timeout}ms"))??;
        inflight.idle(timeout).await;
        drop(inflight);
        let v = iso_eval_function(&page, crate::measure::COLLECT_JS).await?;
        let rec = sessions.record(&session_id).await?;
        let vp = sessions.viewport(&session_id).await;
        let device = format!(
            "{}x{} at {}x{}",
            vp.width,
            vp.height,
            vp.scale,
            if sessions.is_mobile(&session_id).await {
                ", a touch phone"
            } else {
                ""
            }
        );
        let throttling = EmulationOut::from(rec)
            .describe()
            .trim_end_matches('.')
            .to_string();
        let mut merged = v;
        merged["url"] = json!(
            current_url_title(&page)
                .await
                .map(|(u, _)| u)
                .unwrap_or_default()
        );
        merged["conditions"] = json!(format!("{device}; {throttling}"));
        let mut out: crate::measure::Measure =
            serde_json::from_value(merged).context("reading the page's timing records")?;
        out.rate();
        Ok(ok_text_struct(out.render(), out))
    }

    // --- profiling ---

    async fn tab_page(&self, args: &JsonObject) -> Result<Page> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(args, "sessionId")?;
        match optional_u64(args, "tabIndex") {
            Some(idx) => sessions.page_at(session_id, idx as usize).await,
            None => sessions.active_page(session_id).await,
        }
    }

    async fn start_profiling(&self, args: JsonObject) -> Result<Text> {
        let page = self.tab_page(&args).await?;
        let interval = optional_u64(&args, "samplingIntervalUs")
            .map_or(profiler::DEFAULT_INTERVAL_US, |n| n as i64);
        profiler::start(&page, interval).await?;
        Ok(ok_text(format!(
            "CPU profiler running ({interval}µs sampling). Reproduce the problem, then call stop_profiling on the same tab."
        )))
    }

    async fn stop_profiling(&self, args: JsonObject) -> Result<Structured<ProfileOut>> {
        let session_id = required_str(&args, "sessionId")?;
        let page = self.tab_page(&args).await?;
        let timeout = Duration::from_millis(optional_u64(&args, "timeout").unwrap_or(30_000));
        let top = optional_usize(&args, "top").unwrap_or(15);
        let terminate_after = match optional_u64(&args, "terminateAfterMs").unwrap_or(5_000) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        };
        let (profile, terminated) = profiler::stop(&page, timeout, terminate_after).await?;
        let dir = logs::session_dir(&self.logs_dir, session_id)?.join("profiles");
        let path = profiler::write(&dir, &profile).await?;
        let summary = profiler::summarize(&profile, top);
        let mut text = profiler::render(&summary, &path);
        if terminated {
            text.insert_str(0, "Page JS was still running when stopping, so it was terminated to collect the profile (the page was hung; the long task at the end is the culprit).\n");
        }
        Ok(ok_text_struct(
            text,
            ProfileOut {
                summary,
                path,
                terminated,
            },
        ))
    }

    // --- human takeover ---

    async fn request_human_takeover(&self, args: JsonObject) -> Result<Structured<TakeoverOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        // How long the link stays valid, in minutes so no plausible number
        // makes a link that dies before a human can open it. The URL is a
        // bearer credential (it grants live input to the session), so it is
        // capped at an hour.
        if args.contains_key("ttl") {
            return Err(anyhow!(
                "`ttl` was replaced by `ttlMinutes` (1 to 60, default 30)"
            ));
        }
        let ttl_ms = optional_u64(&args, "ttlMinutes").unwrap_or(30).clamp(1, 60) * 60_000;
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
                viewport: sessions.viewport(&session_id).await,
            },
        )
        .await?;
        let url = format!("{base}/takeover/{token}");
        Ok(ok_text_struct(
            format!(
                "Human takeover ready. Show this URL to the user now so they can log in themselves. \
                 Then call await_human_takeover(token=\"{token}\") again and again until completed, \
                 in a background task; with no background task, end your turn and call it once the \
                 user says they're done. The human may take several minutes:\n{url}"
            ),
            TakeoverOut {
                url,
                token,
                expires_at_ms,
            },
        ))
    }

    async fn await_human_takeover(&self, args: JsonObject) -> Result<Structured<TakeoverDoneOut>> {
        let token = required_str(&args, "token")?.to_string();
        // How long to block. Default 25 s, under the per-call limit hosts like
        // Executor set, so a caller polls in a loop instead of having its call
        // cut off; at most 30 min, the longest link.
        let timeout_ms = optional_u64(&args, "timeout")
            .unwrap_or(25_000)
            .min(1_800_000);
        // A token whose ticket is gone was never issued, or its link ended
        // and was already reported.
        let ticket = takeover::read_ticket(&token).await.map_err(|_| {
            anyhow!("unknown takeover token, or its link has ended and was already reported")
        })?;
        let state = takeover::wait_for_handback(&token, Duration::from_millis(timeout_ms)).await?;
        let completed = state != takeover::Handback::Waiting;
        let ended = state == takeover::Handback::Ended;
        let expired = !completed && ticket.expires_at_ms <= takeover::expiry_ms(0);
        let mut where_ = None;
        if completed {
            if ended {
                takeover::cleanup(&token).await;
            }
            if let Ok(sessions) = self.ctx.sessions().await {
                let t = &ticket;
                // A named session saves the login right away.
                if let Some(name) = sessions.name(&t.session_id).await {
                    let _ = self.save_login(&t.session_id, &name).await;
                }
                if let Ok(page) = sessions.active_page(&t.session_id).await {
                    where_ = current_url_title(&page).await;
                }
            }
        }
        let body = if completed {
            let place = match &where_ {
                Some((url, title)) => format!("the session is now on {url} ({title})"),
                None => "resuming".to_string(),
            };
            if ended {
                format!("The person finished and closed the link; {place}.")
            } else {
                format!(
                    "The person handed control back and may keep watching; {place}. If they take \
                     control again, calls on the session fail until they hand back: wait with \
                     await_human_takeover and this token again."
                )
            }
        } else if expired {
            "The link expired before the person finished; issue a new request_human_takeover."
                .to_string()
        } else {
            "Not finished yet; call await_human_takeover again with the same token.".to_string()
        };
        Ok(ok_text_struct(
            body,
            TakeoverDoneOut {
                completed,
                watching: completed && !ended,
                expired,
                url: where_.as_ref().map(|(u, _)| u.clone()),
                title: where_.map(|(_, t)| t),
                token,
            },
        ))
    }

    // --- per-visit logs ---

    async fn list_visits(&self, args: JsonObject) -> Result<Structured<VisitsOut>> {
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
        Ok(ok_text_struct(body, VisitsOut { visits }))
    }

    async fn list_console_messages(&self, args: JsonObject) -> Result<Structured<ConsoleOut>> {
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
        let messages = entries
            .into_iter()
            .filter_map(|e| match e {
                SessionLogEntry::Console(c) => Some(c),
                _ => None,
            })
            .collect();
        Ok(ok_text_struct(body, ConsoleOut { messages }))
    }

    async fn list_network_requests(&self, args: JsonObject) -> Result<Structured<NetworkOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let _ = sessions.context_id_for(&session_id).await?;
        let visit = optional_u32(&args, "visit");
        let limit = optional_usize(&args, "limit").unwrap_or(500);
        let details = optional_bool(&args, "details").unwrap_or(false);
        let url_contains = optional_str(&args, "urlContains").map(str::to_string);
        let mut entries = logs::read_session_logs(
            &self.logs_dir,
            &session_id,
            ReadOpts {
                kind: Some(LogKind::Network),
                limit: if url_contains.is_some() {
                    None
                } else {
                    Some(limit)
                },
                visit,
            },
        )
        .await?;
        if let Some(needle) = &url_contains {
            entries.retain(
                |e| matches!(e, SessionLogEntry::Network(n) if n.url.contains(needle.as_str())),
            );
            let skip = entries.len().saturating_sub(limit);
            entries.drain(..skip);
        }
        // Headers and bodies only when asked: a page makes hundreds of
        // requests, and most questions are about a few.
        if !details {
            for e in &mut entries {
                if let SessionLogEntry::Network(n) = e {
                    n.request_headers = None;
                    n.response_headers = None;
                    n.response_body = None;
                }
            }
        }
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
                        let redirect = match &n.redirected_to {
                            Some(to) => format!("  -> {to}"),
                            None => String::new(),
                        };
                        let mut body = body;
                        if let Some(h) = &n.request_headers {
                            body.push_str(&format!("\n    request headers: {h}"));
                        }
                        if let Some(h) = &n.response_headers {
                            body.push_str(&format!("\n    response headers: {h}"));
                        }
                        if let Some(b) = &n.response_body {
                            let short: String = b.chars().take(2000).collect();
                            let more = if b.chars().count() > 2000 { "…" } else { "" };
                            body.push_str(&format!("\n    response: {short}{more}"));
                        }
                        Some(format!("{} {} [{status}]{redirect}{body}", n.method, n.url))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let requests = entries
            .into_iter()
            .filter_map(|e| match e {
                SessionLogEntry::Network(n) => Some(*n),
                _ => None,
            })
            .collect();
        Ok(ok_text_struct(body, NetworkOut { requests }))
    }

    // --- stealth ---

    async fn set_stealth(&self, args: JsonObject) -> Result<Structured<StealthOut>> {
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
            StealthOut { stealth: enabled },
        ))
    }

    async fn get_stealth(&self, args: JsonObject) -> Result<Structured<StealthOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Validate the session exists, as set_stealth does.
        let _ = sessions.context_id_for(&session_id).await?;
        let enabled = takeover::is_stealth(&session_id).await;
        Ok(ok_text_struct(
            format!("stealth = {enabled}"),
            StealthOut { stealth: enabled },
        ))
    }

    // --- emulation ---

    async fn set_emulation(&self, args: JsonObject) -> Result<Structured<EmulationOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let rate = match args.get("cpuThrottlingRate") {
            None => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|r| r.is_finite() && (1.0..=100.0).contains(r))
                    .ok_or_else(|| anyhow!("cpuThrottlingRate must be a number from 1 to 100"))?,
            ),
        };
        let network = args
            .get("network")
            .map(emulation::parse_network)
            .transpose()?;
        if rate.is_none() && network.is_none() {
            bail!("pass cpuThrottlingRate, network or both");
        }
        let rec = sessions
            .update_emulation(&session_id, |em, _| {
                let em = em.get_or_insert_with(Emulation::default);
                if let Some(rate) = rate {
                    em.cpu_throttling_rate = rate;
                }
                if let Some(network) = network {
                    em.network = network;
                }
            })
            .await?;
        let out = EmulationOut::from(rec);
        Ok(ok_text_struct(out.describe(), out))
    }

    async fn get_emulation(&self, args: JsonObject) -> Result<Structured<EmulationOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let out = EmulationOut::from(sessions.record(&session_id).await?);
        Ok(ok_text_struct(out.describe(), out))
    }

    async fn add_init_script(&self, args: JsonObject) -> Result<Structured<InitScriptOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let source = required_str(&args, "source")?.to_string();
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
        let id: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        let rec = sessions
            .update_emulation(&session_id, |_, scripts| {
                scripts.push(InitScript {
                    id: id.clone(),
                    source,
                })
            })
            .await?;
        Ok(ok_text_struct(
            format!("Added init script {id}; it runs from each tab's next page load."),
            InitScriptOut {
                id,
                session: EmulationOut::from(rec),
            },
        ))
    }

    async fn remove_init_script(&self, args: JsonObject) -> Result<Structured<EmulationOut>> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let id = required_str(&args, "id")?.to_string();
        if !sessions
            .record(&session_id)
            .await?
            .init_scripts
            .iter()
            .any(|s| s.id == id)
        {
            bail!("no init script {id:?} in this session; get_emulation lists them");
        }
        let rec = sessions
            .update_emulation(&session_id, |_, scripts| scripts.retain(|s| s.id != id))
            .await?;
        let out = EmulationOut::from(rec);
        Ok(ok_text_struct(
            format!("Removed init script {id}. {}", out.describe()),
            out,
        ))
    }

    // --- saved browser states ---

    async fn save_state(&self, args: JsonObject) -> Result<Structured<SavedOut>> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        let name = required_str(&args, "name")?.to_string();
        let state = self.save_login(&session_id, &name).await?;
        Ok(ok_text_struct(
            format!("Saved {} cookies as \"{name}\".", state.cookies.len()),
            SavedOut {
                cookie_count: state.cookies.len(),
                name: state.name,
                saved_at: state.saved_at,
            },
        ))
    }

    /// Save a session's cookies under `name`: `save_browser_state`, and a
    /// named session's login when a takeover finishes or it closes.
    async fn save_login(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<crate::saved_states::SavedState> {
        let sessions = self.ctx.sessions().await?;
        let ctx_id = sessions.context_id_for(session_id).await?;
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
        self.saved_states.save(name, cookies_json).await
    }

    /// Load cookies saved under `name` into a session: `load_browser_state`,
    /// and a named session's login when it opens.
    async fn load_login(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<crate::saved_states::SavedState> {
        let sessions = self.ctx.sessions().await?;
        let ctx_id = sessions.context_id_for(session_id).await?;
        let state = self.saved_states.load(name).await?;
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
        Ok(state)
    }

    async fn load_state(&self, args: JsonObject) -> Result<Structured<LoadedOut>> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        let name = required_str(&args, "name")?.to_string();
        let state = self.load_login(&session_id, &name).await?;
        Ok(ok_text_struct(
            format!("Loaded {} cookies from \"{name}\".", state.cookies.len()),
            LoadedOut {
                cookie_count: state.cookies.len(),
                name,
            },
        ))
    }

    async fn list_states(&self, _args: JsonObject) -> Result<Structured<StatesOut>> {
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
        Ok(ok_text_struct(body, StatesOut { states: list }))
    }

    async fn delete_state(&self, args: JsonObject) -> Result<Structured<DeletedOut>> {
        let name = required_str(&args, "name")?.to_string();
        let removed = self.saved_states.delete(&name).await?;
        let msg = if removed {
            format!("Deleted \"{name}\".")
        } else {
            format!("No saved state named \"{name}\".")
        };
        Ok(ok_text_struct(msg, DeletedOut { name, removed }))
    }
}

// ---- helpers --------------------------------------------------------------

#[derive(Debug, Serialize, JsonSchema)]
struct PageSummary {
    /// 0-based index to pass as `tabIndex`.
    index: usize,
    url: String,
    title: String,
    /// Whether this is the tab other tools act on by default.
    active: bool,
}

// ---- tool results ---------------------------------------------------------
//
// Every handler returns a `ToolOutput`, and `tools!` declares each tool's
// outputSchema from its handler's return type, so the schema clients see can't
// drift from the `structuredContent` the handler sends.

trait ToolOutput {
    /// Add this result type's outputSchema, if it has one, to `tool`.
    fn declare(tool: Tool) -> Tool;
    fn into_result(self) -> CallToolResult;
}

/// A text-only result; declares no outputSchema.
struct Text(String);

impl ToolOutput for Text {
    fn declare(tool: Tool) -> Tool {
        tool
    }

    fn into_result(self) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(self.0)])
    }
}

/// Text plus `structuredContent` serialized from `T`, whose JSON Schema is the
/// tool's outputSchema.
struct Structured<T> {
    text: String,
    value: T,
}

impl<T: Serialize + JsonSchema + 'static> ToolOutput for Structured<T> {
    fn declare(tool: Tool) -> Tool {
        tool.with_output_schema::<T>()
    }

    fn into_result(self) -> CallToolResult {
        let mut r = CallToolResult::success(vec![ContentBlock::text(self.text)]);
        r.structured_content =
            Some(serde_json::to_value(self.value).expect("tool results serialize to JSON"));
        r
    }
}

/// A PNG image plus a caption; declares no outputSchema.
struct Screenshot {
    text: String,
    /// None when the image was saved to a file instead.
    png_base64: Option<String>,
    /// Where it was saved, when it was.
    saved: Option<(PathBuf, usize)>,
}

impl ToolOutput for Screenshot {
    fn declare(tool: Tool) -> Tool {
        tool
    }

    fn into_result(self) -> CallToolResult {
        let mut blocks = vec![ContentBlock::text(self.text)];
        if let Some(png) = self.png_base64 {
            blocks.push(ContentBlock::image(png, "image/png".to_string()));
        }
        let mut result = CallToolResult::success(blocks);
        if let Some((path, bytes)) = self.saved {
            result.structured_content = Some(json!({ "path": path, "bytes": bytes }));
        }
        result
    }
}

/// Declare `tool`'s outputSchema from `_handler`'s return type. The handler is
/// never called; it only names the type.
fn declare_output<'a, F, Fut, O>(tool: Tool, _handler: F) -> Tool
where
    F: FnOnce(&'a BrowserSessionServer, JsonObject) -> Fut,
    Fut: std::future::Future<Output = Result<O>>,
    O: ToolOutput,
{
    O::declare(tool)
}

/// The tool table: each tool name and its handler. Generates `dispatch` and
/// `declare_output_for`, so routing and output schemas come from one list.
macro_rules! tools {
    ($($name:literal => $handler:ident,)*) => {
        impl BrowserSessionServer {
            async fn dispatch(&self, name: &str, args: JsonObject) -> Result<CallToolResult> {
                match name {
                    $($name => Ok(self.$handler(args).await?.into_result()),)*
                    other => bail!("unknown tool: {other}"),
                }
            }
        }

        /// Add `tool`'s outputSchema from its handler. Panics on a tool with no
        /// handler, which the `tool_table_matches_definitions` test catches.
        fn declare_output_for(tool: Tool) -> Tool {
            match tool.name.as_ref() {
                $($name => declare_output(tool, BrowserSessionServer::$handler),)*
                other => panic!("tool `{other}` has no handler in tools!"),
            }
        }

        #[cfg(test)]
        const TOOL_NAMES: &[&str] = &[$($name,)*];
    };
}

tools! {
    "open_browser_session" => open_session,
    "close_browser_session" => close_session,
    "list_browser_sessions" => list_sessions,
    "new_page" => new_page,
    "list_pages" => list_pages,
    "switch_tab" => switch_tab,
    "close_tab" => close_tab,
    "navigate" => navigate,
    "take_screenshot" => take_screenshot,
    "take_snapshot" => take_snapshot,
    "act" => act,
    "ask" => ask,
    "read" => read,
    "click" => click,
    "tap" => tap,
    "swipe" => swipe,
    "type" => type_text,
    "select_option" => select_option,
    "upload_file" => upload_file,
    "press_key" => press_key,
    "scroll" => scroll,
    "move_mouse" => move_mouse,
    "wait_for" => wait_for,
    "evaluate" => evaluate,
    "measure_page" => measure_page,
    "start_profiling" => start_profiling,
    "stop_profiling" => stop_profiling,
    "list_visits" => list_visits,
    "list_console_messages" => list_console_messages,
    "list_network_requests" => list_network_requests,
    "set_stealth" => set_stealth,
    "get_stealth" => get_stealth,
    "set_emulation" => set_emulation,
    "get_emulation" => get_emulation,
    "add_init_script" => add_init_script,
    "remove_init_script" => remove_init_script,
    "request_human_takeover" => request_human_takeover,
    "await_human_takeover" => await_human_takeover,
    "save_browser_state" => save_state,
    "load_browser_state" => load_state,
    "list_browser_states" => list_states,
    "delete_browser_state" => delete_state,
}

#[derive(Serialize, JsonSchema)]
struct SessionsOut {
    sessions: Vec<SessionInfo>,
}

#[derive(Serialize, JsonSchema)]
struct TabOut {
    /// The tab's 0-based list_pages index.
    index: usize,
    url: String,
}

#[derive(Serialize, JsonSchema)]
struct PagesOut {
    pages: Vec<PageSummary>,
}

#[derive(Serialize, JsonSchema)]
struct NavigateOut {
    /// The page's URL after any redirects.
    url: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ActStep {
    /// 1-based step number.
    step: usize,
    /// "click", "type", "select", "scroll_down", "scroll_up", "wait",
    /// "back", "press_key" (the key in `target`), "hover", "right_click",
    /// "drag", or "missed" when the element was gone before it could be
    /// acted on.
    action: String,
    /// The element acted on, as it was offered to Jev.
    target: Option<String>,
    /// The `values` key that was typed, when this step typed one.
    typed: Option<String>,
    /// Jev's confidence in this step's choice.
    confidence: f64,
    /// Jev's probability the goal was met, as judged this step.
    goal: f64,
    /// Jev's probability of being stuck, as judged this step.
    stuck: f64,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ActOut {
    /// "completed", "likely_completed", "uncertain", "stuck", "needs_login",
    /// "needs_human", "needs_confirmation", "blocked", "no_progress" or
    /// "max_steps".
    status: String,
    /// The goal as given.
    goal: String,
    /// What was done, in order.
    steps: Vec<ActStep>,
    /// The page's URL at the final decision.
    url: String,
    /// The page's title at the final decision.
    title: String,
    /// For "uncertain" and "likely_completed" stops on a low-confidence
    /// pick: what Jev was choosing between, most likely first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<ActCandidate>,
    /// For "needs_confirmation": the element `act` stopped short of
    /// clicking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<String>,
    /// JavaScript dialogs the page opened, which `act` accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dialogs: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct ActCandidate {
    /// The element or action, as it was offered to Jev.
    target: String,
    /// Jev's probability for it.
    probability: f64,
}

/// The parts of an `act` result only some stops carry.
struct ActExtra {
    candidates: Vec<ActCandidate>,
    pending: Option<String>,
    dialogs: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ReadOut {
    /// The passages kept, in page order, under their headings as `## `
    /// lines; empty when nothing looked relevant.
    text: String,
    url: String,
    title: String,
    /// Visible characters on the page.
    page_chars: usize,
    /// Passages the page was split into.
    passages: usize,
    /// Passages returned.
    kept: usize,
    /// False when the page was short enough to return whole, without Jev.
    filtered: bool,
}

#[derive(Serialize, JsonSchema)]
struct SelectOut {
    /// The option picked, as the list shows it; absent when only listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    chosen: Option<String>,
    /// What the field shows after: a native select's option, or the text
    /// by a custom dropdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    shown: Option<String>,
    /// Whether the field shows the chosen option.
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed: Option<bool>,
    /// The options the dropdown offered.
    options: Vec<String>,
    #[serde(flatten)]
    after: AfterAction,
}

#[derive(Serialize, JsonSchema)]
struct UploadOut {
    /// The file name the page received.
    name: String,
    bytes: usize,
    #[serde(rename = "mimeType")]
    mime_type: String,
    /// "input" when set on a file input, "drop" when dropped on an element.
    how: String,
    /// The file input or drop zone that received it.
    into: String,
    /// Whether the file's name appears on the page after.
    #[serde(rename = "pageShowsName")]
    page_shows_name: bool,
    #[serde(flatten)]
    after: AfterAction,
}

#[derive(Serialize, JsonSchema)]
struct AskOut {
    /// "yes" or "no" for a yes/no question, else the option that fits.
    answer: String,
    /// For a yes/no question, the probability of yes; with options, Jev's
    /// confidence in `answer`.
    probability: f64,
    /// With options: each option's probability, most likely first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    probabilities: Vec<AskOption>,
}

#[derive(Serialize, JsonSchema)]
struct AskOption {
    option: String,
    probability: f64,
}

#[derive(Serialize, JsonSchema)]
struct EvaluateOut {
    /// The expression's value; null for undefined.
    result: Value,
}

#[derive(Serialize, JsonSchema)]
struct ProfileOut {
    #[serde(flatten)]
    summary: profiler::Summary,
    /// Where the .cpuprofile was saved.
    path: PathBuf,
    /// Whether hung page JS was terminated to collect the profile.
    terminated: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TakeoverOut {
    /// The link to show the person who will log in.
    url: String,
    /// Pass to await_human_takeover.
    token: String,
    /// When the link stops working, in milliseconds since the Unix epoch.
    expires_at_ms: u128,
}

#[derive(Serialize, JsonSchema)]
struct TakeoverDoneOut {
    /// True once the person handed control back or the link ended; false
    /// while they still have control.
    completed: bool,
    /// True when they handed back but the link is still open: they may be
    /// watching, and can take control again.
    watching: bool,
    /// True when the link expired unfinished: request a new one.
    expired: bool,
    /// Where the session is once finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    token: String,
}

#[derive(Serialize, JsonSchema)]
struct VisitsOut {
    visits: Vec<logs::VisitHeader>,
}

#[derive(Serialize, JsonSchema)]
struct ConsoleOut {
    messages: Vec<logs::ConsoleEntry>,
}

#[derive(Serialize, JsonSchema)]
struct NetworkOut {
    requests: Vec<logs::NetworkEntry>,
}

#[derive(Serialize, JsonSchema)]
struct StealthOut {
    stealth: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EmulationOut {
    /// CPU slowdown factor; 1 means none.
    cpu_throttling_rate: f64,
    /// Network throttling; null means none.
    network: Option<NetworkConditions>,
    init_scripts: Vec<InitScript>,
}

impl From<SessionRecord> for EmulationOut {
    fn from(rec: SessionRecord) -> Self {
        let em = rec.emulation.unwrap_or_default();
        Self {
            cpu_throttling_rate: em.cpu_throttling_rate.max(1.0),
            network: em.network,
            init_scripts: rec.init_scripts,
        }
    }
}

impl EmulationOut {
    fn describe(&self) -> String {
        let cpu = if self.cpu_throttling_rate > 1.0 {
            format!("CPU {}x slower", self.cpu_throttling_rate)
        } else {
            "CPU unthrottled".into()
        };
        let net = match &self.network {
            None => "network unthrottled".into(),
            Some(n) if n.offline => "network offline".into(),
            Some(n) => format!(
                "network {}{}ms latency, {} kbps down, {} kbps up",
                n.preset
                    .as_deref()
                    .map(|p| format!("{p}: "))
                    .unwrap_or_default(),
                n.latency_ms,
                n.download_kbps,
                n.upload_kbps
            ),
        };
        format!("{cpu}, {net}, {} init script(s).", self.init_scripts.len())
    }
}

#[derive(Serialize, JsonSchema)]
struct InitScriptOut {
    /// Pass to remove_init_script.
    id: String,
    #[serde(flatten)]
    session: EmulationOut,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SavedOut {
    name: String,
    saved_at: String,
    cookie_count: usize,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct LoadedOut {
    name: String,
    cookie_count: usize,
}

#[derive(Serialize, JsonSchema)]
struct StatesOut {
    states: Vec<StateSummary>,
}

#[derive(Serialize, JsonSchema)]
struct DeletedOut {
    name: String,
    /// False if no state had that name.
    removed: bool,
}

fn ok_text(text: impl Into<String>) -> Text {
    Text(text.into())
}

fn ok_text_struct<T>(text: impl Into<String>, value: T) -> Structured<T> {
    Structured {
        text: text.into(),
        value,
    }
}

/// Whether Chrome itself rejected a command (a protocol error response,
/// e.g. "Cannot find context with specified id"), as opposed to a transport
/// failure.
fn is_protocol_error(err: &anyhow::Error) -> bool {
    use chromiumoxide::error::CdpError;
    err.chain().any(|e| {
        matches!(
            e.downcast_ref::<CdpError>(),
            Some(CdpError::Chrome(_) | CdpError::ChromeMessage(_))
        )
    })
}

/// Whether an error came off the CDP transport itself (socket, channel, no
/// reply), judged by chromiumoxide's error type anywhere in the chain. Page
/// errors that merely mention a websocket don't count.
fn is_transport_error(err: &anyhow::Error) -> bool {
    use chromiumoxide::error::CdpError;
    err.chain().any(|e| {
        matches!(
            e.downcast_ref::<CdpError>(),
            Some(
                CdpError::Ws(_)
                    | CdpError::Io(_)
                    | CdpError::NoResponse
                    | CdpError::ChannelSendError(_)
                    | CdpError::Timeout
            )
        )
    })
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
    let scale = match obj.get("deviceScaleFactor") {
        None => 1.0,
        Some(v) => v
            .as_f64()
            .filter(|n| (0.5..=4.0).contains(n))
            .ok_or_else(|| anyhow!("{field}.deviceScaleFactor must be a number from 0.5 to 4"))?,
    };
    Ok(Some(Viewport {
        width,
        height,
        scale,
    }))
}

/// Evaluate a JS *function declaration* in the tab's isolated world and
/// return its JSON result. Once the tool call found its target in a frame
/// page scripts can't reach, it runs there instead (see [`crate::frames`]).
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
///
/// Worlds live as long as their document, so one is created per document and
/// reused (a fresh world per call grew renderer memory without bound under
/// `wait_for` polling on pages that never navigate). Context ids are only
/// unique per renderer process, so after a cross-process navigation a cached
/// id could name a different context, even the page's main world. Each world
/// therefore carries a random marker, and every call checks it before running
/// anything: a missing context or a missing marker means "make a new world".
pub(crate) async fn iso_eval_function(page: &Page, function_declaration: &str) -> Result<Value> {
    match crate::frames::current() {
        Some(frame) => crate::frames::eval(&frame, function_declaration).await,
        None => iso_eval_top(page, function_declaration).await,
    }
}

/// [`iso_eval_function`] in the tab's top document, whichever frame the tool
/// call works in: for the page's own URL, title, scroll and history.
pub(crate) async fn iso_eval_top(page: &Page, function_declaration: &str) -> Result<Value> {
    let frame_id = page
        .mainframe()
        .await?
        .ok_or_else(|| anyhow!("no main frame to evaluate in"))?;
    let key = format!("{}:{}", page.target_id().as_ref(), frame_id.as_ref());
    let cached = ISO_WORLDS
        .lock()
        .expect("iso worlds lock")
        .get(&key)
        .cloned();
    if let Some(world) = cached {
        // A CDP error (context gone after a navigation) or a missing marker
        // (the id now names another context) means stale: fall through to a
        // fresh world. A JS result or exception is final — the code ran, and
        // must not run twice.
        match call_in_world(page, &world, function_declaration).await {
            Ok(Some(outcome)) => return outcome,
            Ok(None) => {}
            // Chrome rejected the context id: stale. A transport error, by
            // contrast, may have run the code already, so it propagates.
            Err(e) if is_protocol_error(&e) => {}
            Err(e) => return Err(e),
        }
    }
    let created = page
        .execute(
            CreateIsolatedWorldParams::builder()
                .frame_id(frame_id)
                .grant_univeral_access(true)
                .build()
                .map_err(|e| anyhow!("CreateIsolatedWorld: {e}"))?,
        )
        .await?;
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
    let world = IsoWorld {
        context_id: created.result.execution_context_id,
        marker: buf.iter().map(|b| format!("{b:02x}")).collect(),
    };
    let mark = format!(
        "function() {{ globalThis._{} = true; return true; }}",
        world.marker
    );
    raw_call(page, world.context_id, &mark).await??;
    {
        let mut worlds = ISO_WORLDS.lock().expect("iso worlds lock");
        // Entries for closed tabs are never looked up again; a coarse reset
        // keeps the map bounded without tracking tab lifetimes here.
        if worlds.len() >= 1024 {
            worlds.clear();
        }
        worlds.insert(key, world.clone());
    }
    call_in_world(page, &world, function_declaration)
        .await?
        .ok_or_else(|| anyhow!("the new isolated world lost its marker"))?
}

/// One cached isolated world: its context id and the marker set inside it
/// (a random global name, so even a check that lands in the page's main
/// world reads a property no page script can know to trap).
#[derive(Clone)]
struct IsoWorld {
    context_id: chromiumoxide::cdp::js_protocol::runtime::ExecutionContextId,
    marker: String,
}

/// Isolated worlds by `target:frame`, for [`iso_eval_function`].
static ISO_WORLDS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, IsoWorld>>,
> = once_cell::sync::Lazy::new(Default::default);

/// Sentinel the marker check returns when the context isn't our world.
const STALE_WORLD: &str = "__bsw_stale__";

/// Run `decl` in `world` behind the marker check. The outer `Err` is a CDP
/// failure; `None` means the context isn't ours (anymore), and nothing ran;
/// `Some` carries the JS outcome, an exception as its `Err`.
async fn call_in_world(page: &Page, world: &IsoWorld, decl: &str) -> Result<Option<Result<Value>>> {
    let guarded = format!(
        // Newlines around `decl`, so a trailing `// comment` in caller code
        // can't swallow the closing paren.
        "function() {{ if (globalThis._{marker} !== true) return '{STALE_WORLD}';\n\
         return (\n{decl}\n).apply(this, arguments); }}",
        marker = world.marker,
    );
    Ok(match raw_call(page, world.context_id, &guarded).await? {
        Ok(Value::String(s)) if s == STALE_WORLD => None,
        outcome => Some(outcome),
    })
}

/// One `Runtime.callFunctionOn` by context id. The outer `Result` is the CDP
/// call; the inner one is the JS outcome (an exception becomes `Err`).
async fn raw_call(
    page: &Page,
    context_id: chromiumoxide::cdp::js_protocol::runtime::ExecutionContextId,
    decl: &str,
) -> Result<Result<Value>> {
    let call = CallFunctionOnParams::builder()
        .function_declaration(decl)
        .execution_context_id(context_id)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| anyhow!("CallFunctionOn builder: {e}"))?;
    let resp = page.execute(call).await?;
    if let Some(exc) = resp.result.exception_details.as_ref() {
        return Ok(Err(anyhow!("JS exception: {}", exception_message(exc))));
    }
    Ok(Ok(resp.result.result.value.clone().unwrap_or(Value::Null)))
}

/// Turn `evaluate` input into an async function declaration. Bare expressions
/// get an implicit `return`; anything that doesn't parse that way (statements,
/// an explicit `return`) runs as a function body. Deciding by parsing rather
/// than by spotting the word `return` keeps `arr.map(x => { return x * 2 })`
/// returning its value.
///
/// The parse check nests the candidate in a function that is declared but
/// never called: the whole declaration must parse, so a syntax error still
/// throws, yet none of the input runs (it can't run twice, and a runtime
/// `SyntaxError` like `JSON.parse('x')` isn't mistaken for a parse failure).
/// `Runtime.compileScript` would be cleaner but requires `Runtime.enable`.
async fn wrap_expression(page: &Page, expression: &str, stealth: bool) -> Result<String> {
    let trimmed = expression.trim().trim_end_matches(';').trim_end();
    // Newlines so a trailing `// comment` can't swallow the closing paren.
    let body = format!("return (\n{trimmed}\n);");
    let probe = format!("function() {{ return true; async function probe() {{ {body} }} }}");
    let parses = if stealth {
        iso_eval_function(page, &probe).await
    } else {
        main_world_eval(page, &probe).await
    }
    .is_ok();
    Ok(if parses {
        format!("async function() {{ {body} }}")
    } else {
        format!("async function() {{\n{expression}\n}}")
    })
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
        bail!("JS exception: {}", exception_message(exc));
    }
    Ok(resp.result.result.value.clone().unwrap_or(Value::Null))
}

/// Resolve a `selector` argument to a CSS selector and wait up to
/// `timeout_ms` for the element to appear. Three routes:
///
/// 1. A `jev:` prefix goes straight to the Jev decision model (`src/jev.rs`),
///    which picks one of the page's interactive elements for the goal.
/// 2. Anything else is used as a CSS selector, as-is.
/// 3. Automatic fallback: if that selector matches nothing within
///    `timeout_ms` and a TypeSafe API key is configured, the same string is
///    retried as a plain-language goal via Jev — so `selector: "the login
///    button"` works with no special syntax. The fallback only fires after
///    the wait expires (an element that merely loads slowly is still found
///    as CSS), and it fails closed: on low confidence or any Jev problem the
///    error names both attempts rather than guessing.
///
/// Returns the CSS selector to act on and, when Jev picked the element, a
/// note for the confirmation text naming what was chosen.
async fn resolve_and_wait(
    page: &Page,
    selector: &str,
    timeout_ms: u64,
) -> Result<(String, Option<String>)> {
    resolve_and_wait_for(page, selector, timeout_ms, None, false).await
}

/// [`resolve_and_wait`] for a tool that can only use one kind of element:
/// `purpose` ("to attach a file to") goes to Jev with the words, so "Resume"
/// means the resume upload, not an "Import resume" button beside it. With
/// `by_group`, any button of a radio group stands for the group.
async fn resolve_and_wait_for(
    page: &Page,
    selector: &str,
    timeout_ms: u64,
    purpose: Option<&str>,
    by_group: bool,
) -> Result<(String, Option<String>)> {
    let with_purpose = |goal: &str| match purpose {
        Some(p) => format!("{goal} ({p})"),
        None => goal.to_string(),
    };
    if let Some(goal) = selector.strip_prefix("jev:") {
        let goal = goal.trim();
        if goal.is_empty() {
            bail!("`jev:` needs a goal after the colon, e.g. `jev: the login button`");
        }
        let res = jev::resolve(page, &with_purpose(goal), by_group).await?;
        // Extraction only offers currently-present elements, so this is an
        // instant match; the wait just covers a re-render in between.
        wait_for_selector(page, &res.css, timeout_ms).await?;
        let note = format!(
            "{} (jev: \"{goal}\", confidence {:.2}, {} offered)",
            res.description, res.confidence, res.offered
        );
        return Ok((res.css, Some(note)));
    }
    // Prose can never match as CSS, so it gets one immediate check instead
    // of waiting out the timeout before Jev is asked.
    let css_wait = if jev::available() && looks_like_prose(selector) {
        0
    } else {
        timeout_ms
    };
    if let Err(css_err) = wait_for_selector(page, selector, css_wait).await {
        if !jev::available() {
            return Err(css_err);
        }
        let res = match jev::resolve_fallback(page, &with_purpose(selector), by_group).await {
            Ok(res) => res,
            // Keep the CSS timeout visible: it explains why Jev was tried.
            Err(jev_err) => bail!("{css_err}; jev fallback: {jev_err}"),
        };
        wait_for_selector(page, &res.css, timeout_ms).await?;
        let note = format!(
            "{} (jev fallback for \"{selector}\", confidence {:.2}, {} offered)",
            res.description, res.confidence, res.offered
        );
        return Ok((res.css, Some(note)));
    }
    Ok((selector.to_string(), None))
}

/// Whether a selector is plain words rather than CSS. A CSS type selector
/// names an HTML element or a custom element, and custom element names
/// always contain a hyphen; so a word that is neither, and has no CSS
/// punctuation, can never match ("the login button", "Submit"). "form
/// button" and "#nav a" stay CSS.
fn looks_like_prose(selector: &str) -> bool {
    const TAGS: &[&str] = &[
        "a",
        "abbr",
        "address",
        "area",
        "article",
        "aside",
        "audio",
        "b",
        "base",
        "bdi",
        "bdo",
        "blockquote",
        "body",
        "br",
        "button",
        "canvas",
        "caption",
        "cite",
        "code",
        "col",
        "colgroup",
        "data",
        "datalist",
        "dd",
        "del",
        "details",
        "dfn",
        "dialog",
        "div",
        "dl",
        "dt",
        "em",
        "embed",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "head",
        "header",
        "hgroup",
        "hr",
        "html",
        "i",
        "iframe",
        "img",
        "input",
        "ins",
        "kbd",
        "label",
        "legend",
        "li",
        "link",
        "main",
        "map",
        "mark",
        "menu",
        "meta",
        "meter",
        "nav",
        "noscript",
        "object",
        "ol",
        "optgroup",
        "option",
        "output",
        "p",
        "picture",
        "pre",
        "progress",
        "q",
        "rp",
        "rt",
        "ruby",
        "s",
        "samp",
        "script",
        "search",
        "section",
        "select",
        "slot",
        "small",
        "source",
        "span",
        "strong",
        "style",
        "sub",
        "summary",
        "sup",
        "svg",
        "table",
        "tbody",
        "td",
        "template",
        "textarea",
        "tfoot",
        "th",
        "thead",
        "time",
        "title",
        "tr",
        "track",
        "u",
        "ul",
        "var",
        "video",
        "wbr",
    ];
    selector.split_whitespace().any(|word| {
        let w = word.trim_end_matches(['.', ',', '!', '?']).to_lowercase();
        !w.is_empty()
            && !w.contains([
                '#', '.', '[', ']', '>', ':', '=', '+', '~', '*', '(', ')', '|', '-',
            ])
            && !TAGS.contains(&w.as_str())
    })
}

/// `act` stop gates: Jev's probability that the goal is already met — or
/// that no action can make progress — must cross this before the loop stops
/// on it. 0.85 matches what the jev-browser reference uses for both gates.
const ACT_GATE: f64 = 0.85;

/// `act` stops as "likely_completed" once Jev's goal-met probability reaches
/// this after at least one step, and reports it instead of a plain stop.
/// Achieved goals measured 0.52 to 0.83 in the benchmark, under
/// [`ACT_GATE`], and the loop then acted past them and undid them.
const ACT_LIKELY_GATE: f64 = 0.5;

/// With `confirmIrreversible`, `act` stops before a click Jev judges at
/// least this likely to be impossible to undo.
const ACT_IRREVERSIBLE_GATE: f64 = 0.6;

/// How many times in a row `act` waits on a page with no interactive
/// elements (usually mid-navigation) before stopping as stuck.
const ACT_EMPTY_RETRIES: usize = 3;

/// Pause between executed `act` steps so the page can settle (navigation,
/// form submits) before the next extraction.
const ACT_SETTLE_MS: u64 = 300;

/// `act` wall gates: login walls and error pages usually read clearly off
/// the visible text, so a coin-flip probability is enough to stop and
/// report instead of clicking around a wall we can't pass.
const ACT_WALL_GATE: f64 = 0.5;

/// `act` gives up as `no_progress` after this many consecutive element
/// actions with no visible effect (an unchanged page fingerprint). Before then the
/// loop already retries with Jev's next-best option.
const ACT_NO_EFFECT_LIMIT: usize = 3;

/// Compact one-line step history for `act` error messages, so a mid-loop
/// failure still tells the caller what already happened.
fn trail(steps: &[ActStep]) -> String {
    if steps.is_empty() {
        return "no steps had run yet".to_string();
    }
    steps
        .iter()
        .map(|s| format!("{}:{}", s.step, s.action))
        .collect::<Vec<_>>()
        .join(", ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Final `act` output: a one-line verdict plus the structured trace.
fn act_done(
    status: &str,
    goal: &str,
    steps: Vec<ActStep>,
    url: String,
    title: String,
    extra: ActExtra,
) -> Structured<ActOut> {
    let n = steps.len();
    let (verdict, hint) = match status {
        "completed" if n == 0 => ("Goal was already met".to_string(), None),
        "completed" => (
            format!("Completed the goal in {n} step{}", plural(n)),
            Some("Jev can be eager to call a goal met — verify the page."),
        ),
        "likely_completed" => (
            format!("Probably completed the goal in {n} step{}", plural(n)),
            Some("Jev leaned toward the goal being met but wasn't sure; check the page."),
        ),
        "uncertain" => (
            format!(
                "Stopped after {n} step{}: Jev wasn't sure enough of the next step",
                plural(n)
            ),
            Some(
                "See `candidates` for what it was weighing; act on one yourself or rephrase the goal.",
            ),
        ),
        "needs_human" => (
            format!(
                "Stopped after {n} step{}: the page is a bot check",
                plural(n)
            ),
            Some("Call request_human_takeover so a person can pass it, then call act again."),
        ),
        "needs_confirmation" => (
            format!(
                "Stopped after {n} step{} before an action that can't be undone",
                plural(n)
            ),
            Some(
                "`pending` names it. Click it yourself, or call act again without confirmIrreversible.",
            ),
        ),
        "stuck" => (
            format!("Stuck after {n} step{}", plural(n)),
            Some("Nothing on the page looked able to make progress; take_snapshot and reconsider."),
        ),
        "needs_login" => (
            format!(
                "Stopped after {n} step{}: a sign-in wall is blocking the goal",
                plural(n)
            ),
            Some("Call request_human_takeover, or load a saved login with load_browser_state."),
        ),
        "blocked" => (
            format!(
                "Stopped after {n} step{}: the page showed an error message",
                plural(n)
            ),
            Some("Check list_console_messages / list_network_requests for what failed."),
        ),
        "no_progress" => (
            format!(
                "Stopped after {n} step{}: the last actions had no visible effect",
                plural(n)
            ),
            Some("The page may need a scroll, a wait, or a different goal phrasing."),
        ),
        _ => (
            format!("Stopped at maxSteps with {n} step{} taken", plural(n)),
            Some("Increase maxSteps or continue the goal in a follow-up call."),
        ),
    };
    let text = match hint {
        Some(h) => format!("{verdict}. {h}"),
        None => verdict,
    };
    ok_text_struct(
        text,
        ActOut {
            status: status.to_string(),
            goal: goal.to_string(),
            steps,
            url,
            title,
            candidates: extra.candidates,
            pending: extra.pending,
            dialogs: extra.dialogs,
        },
    )
}

/// Scroll by a fraction of the viewport (positive = down). Used by `act`'s
/// scroll pseudo-actions.
async fn scroll_viewport(page: &Page, fraction: f64) -> Result<()> {
    let decl = format!(
        "function() {{ window.scrollBy(0, Math.round(window.innerHeight * ({fraction}))); return true; }}"
    );
    iso_eval_top(page, &decl).await?;
    Ok(())
}

/// Set a `<select>` to its option at `index`, firing input/change so page
/// scripts observe it. The element is targeted by its extraction
/// reference, and the option by index — labels are clamped to 60 chars
/// for Jev, so matching on text could miss long ones.
async fn set_select_option(page: &Page, css: &str, index: u64, label: &str) -> Result<()> {
    let decl = format!(
        "function() {{ {deep} {native} const h = __bsDeep({css}); if (!h) return false; const el = __bsSelect(h.el); \
         if (!el) return false; const o = el.options[{index}]; if (!o) return false; el.value = o.value; \
         el.dispatchEvent(new Event('input', {{bubbles: true}})); \
         el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }}",
        deep = jev::DEEP_FIND_JS,
        native = NATIVE_SELECT_JS,
        css = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
    );
    let v = iso_eval_function(page, &decl).await?;
    if v.as_bool() != Some(true) {
        bail!("option {index} ({label:?}) is not in the dropdown");
    }
    Ok(())
}

/// Sets `__bsSelect(el)`: the native `<select>` an element stands for (the
/// element itself, the control of a `<label>`, or one inside a wrapper), or
/// null for a custom dropdown.
const NATIVE_SELECT_JS: &str = r#"var __bsSelect = function(el) {
  if (!el) return null;
  if (el.tagName === 'SELECT') return el;
  if (el.tagName === 'LABEL' && el.control && el.control.tagName === 'SELECT') return el.control;
  return (el.querySelector && el.querySelector('select')) || null;
};"#;

/// Where a custom dropdown's options are kept in the isolated world, for
/// `jev-ref:` lookups (see DEEP_FIND_JS).
const OPTION_PICKS: &str = "5e1";

/// Under these keys select_option keeps a radio group's options: where to
/// click each (its label when the button itself is hidden), and the buttons.
const RADIO_TARGETS: &str = "5e3";
const RADIO_INPUTS: &str = "5e4";

/// `__bsRadios(el)`: the options of the radio group `el` is in or is (a
/// radio, its label, a fieldset or a `role=radiogroup`), as
/// `{ input, target, label }`, or null when it isn't one.
const RADIOS_JS: &str = r#"var __bsRadios = function(el) {
  if (!el) return null;
  var isOpt = function(n) { return (n.tagName === 'INPUT' && n.type === 'radio') || (n.getAttribute && n.getAttribute('role') === 'radio'); };
  if (el.tagName === 'LABEL' && el.control && el.control.type === 'radio') el = el.control;
  var list = null;
  if (isOpt(el)) {
    var g = el.getAttribute('role') === 'radio' ? el.closest('[role=radiogroup]') : null;
    if (g) list = g.querySelectorAll('[role=radio]');
    else if (el.name) {
      var root = el.form || el.getRootNode();
      list = Array.prototype.filter.call(root.querySelectorAll('input[type=radio]'), function(r) { return r.name === el.name; });
    } else list = [el];
  } else if (el.matches && el.matches('fieldset, [role=radiogroup], [role=group]')) {
    list = el.querySelectorAll('input[type=radio], [role=radio]');
  }
  if (!list || !list.length) return null;
  return Array.prototype.map.call(list, function(r) {
    var lab = r.labels && r.labels[0];
    var text = (lab && lab.innerText) || r.getAttribute('aria-label') || '';
    if (!text && r.getAttribute('role') === 'radio') text = r.innerText;
    if (!text) {
      var nx = r.nextSibling;
      while (nx && nx.nodeType === 3 && !nx.textContent.trim()) nx = nx.nextSibling;
      text = nx ? (nx.nodeType === 3 ? nx.textContent : nx.innerText || '') : '';
    }
    var rr = r.getBoundingClientRect(), st = getComputedStyle(r);
    var hidden = rr.width < 4 || rr.height < 4 || st.opacity === '0' || st.visibility === 'hidden';
    return { input: r, target: hidden && lab ? lab : r, label: String(text || r.value || '').replace(/\s+/g, ' ').trim() };
  });
};"#;

/// Where `opening_point` keeps the box it clicks, likewise.
const OPENER_PICK: &str = "5e2";

/// Remembers the elements showing in a dropdown's document before it is
/// opened, so `OPTIONS_JS` can tell which ones the opening click revealed.
const MARK_SHOWING_JS: &str = r#"function() { @@DEEP@@
  var h = __bsDeep(@@SEL@@), doc = h ? h.el.ownerDocument : document;
  var win = doc.defaultView || window, seen = new WeakSet(), all = doc.querySelectorAll('*');
  for (var i = 0; i < all.length && i < 20000; i++) { var r = all[i].getBoundingClientRect();
    if (r.width > 0 && r.height > 0 && r.right > 0 && r.bottom > 0 && r.left < win.innerWidth && r.top < win.innerHeight) seen.add(all[i]); }
  globalThis.__bsShowing = seen;
  return true;
}"#;

/// The options a custom dropdown shows now, as text, kept under
/// `OPTION_PICKS` in order. They are the `role=option` elements of the list
/// the field's `aria-controls`/`aria-owns` names (on it, inside it or on
/// the focused element), else every visible one in the field's document
/// and its open shadow roots (menus are often drawn in a portal at the end
/// of the body). A dropdown without those roles (older libraries, hand
/// made ones) gets the entries of the box that gained the most visible
/// entries with text since `MARK_SHOWING_JS` ran.
const OPTIONS_JS: &str = r#"function() { @@DEEP@@
  var h = __bsDeep(@@SEL@@), el = h && h.el, doc = el ? el.ownerDocument : document;
  var win = doc.defaultView || window;
  var vis = function(o) { var r = o.getBoundingClientRect(); return r.width > 0 && r.height > 0 && win.getComputedStyle(o).visibility !== 'hidden'; };
  var label = function(o) { return String(o.innerText || o.textContent || o.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim(); };
  var lists = [];
  [el, el && el.querySelector && el.querySelector('[role=combobox], input'), doc.activeElement].forEach(function(n) {
    if (!n || !n.getAttribute) return;
    ((n.getAttribute('aria-controls') || '') + ' ' + (n.getAttribute('aria-owns') || '')).split(/\s+/).forEach(function(id) {
      var root = n.getRootNode();
      var l = id && (root.getElementById ? root.getElementById(id) : doc.getElementById(id));
      if (l && lists.indexOf(l) < 0) lists.push(l);
    });
  });
  var opts = [];
  var take = function(root) { root.querySelectorAll('[role=option]').forEach(function(o) { if (vis(o) && opts.indexOf(o) < 0) opts.push(o); }); };
  lists.forEach(take);
  if (!opts.length) {
    var walk = function(root, depth) { take(root); if (depth > 3) return;
      root.querySelectorAll('*').forEach(function(n) { if (n.shadowRoot) walk(n.shadowRoot, depth + 1); }); };
    walk(doc, 0);
  }
  var seen = globalThis.__bsShowing;
  if (!opts.length && seen) {
    // Shown on the screen now: a closed menu is often only moved off it.
    var onScreen = function(n) { var r = n.getBoundingClientRect();
      return vis(n) && r.right > 0 && r.bottom > 0 && r.left < win.innerWidth && r.top < win.innerHeight; };
    var entry = function(n) { var t = label(n); return t && t.length <= 200; };
    var fresh = function(n) { return !seen.has(n) && onScreen(n) && entry(n); };
    var best = null, most = 0;
    var all = doc.querySelectorAll('*');
    for (var i = 0; i < all.length && i < 20000; i++) {
      var n = Array.prototype.filter.call(all[i].children, fresh).length;
      if (n >= 2 && n > most) { best = all[i]; most = n; }
    }
    // Every entry of that box, including ones scrolled out of its view.
    if (best) opts = Array.prototype.filter.call(best.children, function(n) { return vis(n) && entry(n); });
  }
  var store = globalThis.__bsPicks || (globalThis.__bsPicks = { order: [] });
  store[@@KEY@@] = opts;
  return opts.map(label);
}"#;

/// What a dropdown shows as its value: a native select's chosen option, or
/// the text nearest the field (its own value, then its ancestors' text up
/// to four levels, leaving out any open option list), and whether `want`
/// is in it.
const SHOWN_JS: &str = r#"function() { @@DEEP@@ @@NATIVE@@ @@RADIOS@@
  var h = __bsDeep(@@SEL@@), el = h && h.el; if (!el) return null;
  var want = @@WANT@@.toLowerCase();
  var rs = __bsRadios(el);
  if (rs) {
    var on = rs.filter(function(r) { return r.input.checked === true || r.input.getAttribute('aria-checked') === 'true'; })[0];
    var picked = on ? on.label : '';
    return { shown: picked, has: picked.toLowerCase() === want };
  }
  var sel = __bsSelect(el);
  if (sel) { var o = sel.selectedOptions[0]; var t = o ? o.text.trim() : ''; return { shown: t, has: t.toLowerCase() === want }; }
  var text = function(n) {
    var out = '', w = document.createTreeWalker(n, NodeFilter.SHOW_TEXT);
    while (w.nextNode()) { var p = w.currentNode.parentElement;
      if (p && p.closest('[role=listbox], [role=option]')) continue;
      if (p && getComputedStyle(p).visibility === 'hidden') continue;
      out += ' ' + w.currentNode.nodeValue; }
    return out.replace(/\s+/g, ' ').trim();
  };
  var first = '';
  if (el.value) { first = String(el.value).trim(); if (first.toLowerCase().indexOf(want) >= 0) return { shown: first, has: true }; }
  for (var n = el, i = 0; n && n !== document.body && i < 5; n = n.parentElement, i++) {
    var t = text(n);
    if (!t || t.length > 300) continue;
    if (!first) first = t;
    if (t.toLowerCase().indexOf(want) >= 0) return { shown: t, has: true };
  }
  return { shown: first, has: false };
}"#;

/// The index of the option `want` names: an exact match (ignoring case and
/// spacing), else the shortest option that starts with it, else the
/// shortest that contains it ("United States" picks "United States +1"
/// over "United States Minor Outlying Islands +1").
fn pick_option(options: &[String], want: &str) -> Option<usize> {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let want = norm(want);
    if want.is_empty() {
        return None;
    }
    let opts: Vec<String> = options.iter().map(|o| norm(o)).collect();
    if let Some(i) = opts.iter().position(|o| *o == want) {
        return Some(i);
    }
    let shortest = |f: &dyn Fn(&str) -> bool| {
        opts.iter()
            .enumerate()
            .filter(|(_, o)| f(o))
            .min_by_key(|(_, o)| o.len())
            .map(|(i, _)| i)
    };
    shortest(&|o| o.starts_with(&want)).or_else(|| shortest(&|o| o.contains(&want)))
}

/// A short list of options for an error message.
fn option_list(options: &[String]) -> String {
    let mut s = options
        .iter()
        .take(30)
        .map(|o| format!("{o:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    if options.len() > 30 {
        s.push_str(&format!(" and {} more", options.len() - 30));
    }
    s
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "''".to_string())
}

/// A native select's option labels, or `None` for a custom dropdown.
async fn native_options(page: &Page, css: &str) -> Result<Option<Vec<String>>> {
    let decl = format!(
        "function() {{ {deep} {native} var h = __bsDeep({sel}); var s = h && __bsSelect(h.el); \
         return s ? Array.from(s.options).map(function(o) {{ return o.text.replace(/\\s+/g, ' ').trim(); }}) : null; }}",
        deep = jev::DEEP_FIND_JS,
        native = NATIVE_SELECT_JS,
        sel = js_str(css),
    );
    let v = iso_eval_function(page, &decl).await?;
    Ok(v.as_array().map(|a| {
        a.iter()
            .map(|o| o.as_str().unwrap_or_default().to_string())
            .collect()
    }))
}

/// The options a custom dropdown shows, polled for up to `wait_ms` until
/// one matches `want` (or, with no `want`, until any show), else whatever
/// is showing by then.
async fn shown_options(page: &Page, css: &str, want: Option<&str>, wait_ms: u64) -> Vec<String> {
    let decl = OPTIONS_JS
        .replace("@@DEEP@@", jev::DEEP_FIND_JS)
        .replace("@@KEY@@", &js_str(OPTION_PICKS))
        .replace("@@SEL@@", &js_str(css));
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let opts: Vec<String> = iso_eval_function(page, &decl)
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let done = match want {
            Some(w) => pick_option(&opts, w).is_some(),
            None => !opts.is_empty(),
        };
        if done || tokio::time::Instant::now() >= deadline {
            return opts;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What the dropdown at `css` shows, and whether it is `want`.
async fn shown_value(page: &Page, css: &str, want: &str) -> (String, bool) {
    let decl = SHOWN_JS
        .replace("@@DEEP@@", jev::DEEP_FIND_JS)
        .replace("@@NATIVE@@", NATIVE_SELECT_JS)
        .replace("@@RADIOS@@", RADIOS_JS)
        .replace("@@SEL@@", &js_str(css))
        .replace("@@WANT@@", &js_str(want));
    let v = iso_eval_function(page, &decl).await.unwrap_or(Value::Null);
    (
        v.get("shown")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        v.get("has").and_then(Value::as_bool).unwrap_or(false),
    )
}

/// Whether the focused element is a dropdown you can search: it takes
/// typing and says it has a list (`role=combobox`, `aria-autocomplete`,
/// `aria-haspopup`, `aria-expanded` or a `list` attribute), so typing into
/// a plain text field never happens by mistake.
async fn focused_is_combobox(page: &Page) -> bool {
    iso_eval_function(
        page,
        "function() { var el = document.activeElement; \
         while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement; \
         var editable = !!el && (el.isContentEditable || \
           (el.tagName === 'INPUT' && !/^(button|checkbox|radio|submit|reset|file|image|hidden)$/i.test(el.type))); \
         return editable && (el.getAttribute('role') === 'combobox' || el.hasAttribute('aria-autocomplete') || \
           el.hasAttribute('aria-haspopup') || el.hasAttribute('aria-expanded') || el.hasAttribute('list')); }",
    )
    .await
    .ok()
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
}

/// Lists drawn with fewer options than this show all they have, so typing
/// to search them can't find anything new; longer ones may be drawn in part.
const PARTIAL_LIST_MIN: usize = 20;

/// What `choose_option` saw and did.
struct Choice {
    /// The option picked; `None` when only listing.
    chosen: Option<String>,
    /// The options the dropdown showed.
    options: Vec<String>,
}

/// Pick `want` in the dropdown at `css`, or with `want` `None` only list its
/// options. A native select is read and set directly. A custom one is
/// opened with a real click; when the option isn't showing and the list
/// may be partial (empty, still loading, or long), its text is typed to
/// filter, and cleared again if nothing matches; then the option is
/// clicked. Listing closes the dropdown again without typing.
/// A radio group's option labels, keeping where to click each and its
/// button for [`pick_radio`]; `None` when `css` isn't a radio group.
async fn radio_options(page: &Page, css: &str) -> Result<Option<Vec<String>>> {
    let decl = format!(
        "function() {{ {deep} {radios} var h = __bsDeep({sel}); var rs = h && __bsRadios(h.el); if (!rs) return null; \
         var p = globalThis.__bsPicks || (globalThis.__bsPicks = {{ order: [] }}); \
         p[{targets}] = rs.map(function(r) {{ return r.target; }}); p[{inputs}] = rs.map(function(r) {{ return r.input; }}); \
         return rs.map(function(r) {{ return r.label; }}); }}",
        deep = jev::DEEP_FIND_JS,
        radios = RADIOS_JS,
        sel = js_str(css),
        targets = js_str(RADIO_TARGETS),
        inputs = js_str(RADIO_INPUTS),
    );
    let v = iso_eval_function(page, &decl).await?;
    Ok(v.as_array().map(|a| {
        a.iter()
            .map(|o| o.as_str().unwrap_or_default().to_string())
            .collect()
    }))
}

/// Click the `i`th option [`radio_options`] found, as a person would, and
/// check that it is now the selected one.
async fn pick_radio(page: &Page, i: usize, label: &str, option: &str) -> Result<()> {
    let target = format!("jev-ref:{RADIO_TARGETS}:{i}");
    let at = element_center(page, &target).await?;
    check_point(page, at, Some(&target), label).await?;
    page.click(Point::new(at.0, at.1))
        .await
        .with_context(|| format!("choosing {option:?} in {label}"))?;
    let checked = format!(
        "function() {{ var p = globalThis.__bsPicks; var r = p && p[{inputs}] && p[{inputs}][{i}]; \
         return !!r && (r.checked === true || r.getAttribute('aria-checked') === 'true'); }}",
        inputs = js_str(RADIO_INPUTS),
    );
    let ok = iso_eval_function(page, &checked)
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !ok {
        bail!("clicked {option:?} in {label}, but it isn't selected afterwards");
    }
    Ok(())
}

async fn choose_option(page: &Page, css: &str, label: &str, want: Option<&str>) -> Result<Choice> {
    if let Some(opts) = radio_options(page, css).await? {
        let Some(want) = want else {
            return Ok(Choice {
                chosen: None,
                options: opts,
            });
        };
        let i = pick_option(&opts, want).ok_or_else(|| {
            anyhow!(
                "{label} has no option {want:?}; it offers {}",
                option_list(&opts)
            )
        })?;
        pick_radio(page, i, label, &opts[i]).await?;
        return Ok(Choice {
            chosen: Some(opts[i].clone()),
            options: opts,
        });
    }
    if let Some(opts) = native_options(page, css).await? {
        let Some(want) = want else {
            return Ok(Choice {
                chosen: None,
                options: opts,
            });
        };
        let i = pick_option(&opts, want).ok_or_else(|| {
            anyhow!(
                "{label} has no option {want:?}; it offers {}",
                option_list(&opts)
            )
        })?;
        set_select_option(page, css, i as u64, &opts[i]).await?;
        return Ok(Choice {
            chosen: Some(opts[i].clone()),
            options: opts,
        });
    }
    let ((x, y), opener) = opening_point(page, css).await?;
    check_point(page, (x, y), Some(&opener), label).await?;
    let mark = MARK_SHOWING_JS
        .replace("@@DEEP@@", jev::DEEP_FIND_JS)
        .replace("@@SEL@@", &js_str(css));
    iso_eval_function(page, &mark).await.ok();
    page.click(Point::new(x, y))
        .await
        .with_context(|| format!("opening {label}"))?;
    let mut opts = shown_options(page, css, want, 1_000).await;
    if opts.is_empty() {
        // A page still starting up can ignore the first click; look once
        // more in case the list was only slow, then click again.
        opts = shown_options(page, css, want, 300).await;
        if opts.is_empty() {
            iso_eval_function(page, &mark).await.ok();
            page.click(Point::new(x, y))
                .await
                .with_context(|| format!("opening {label}"))?;
            opts = shown_options(page, css, want, 1_000).await;
        }
    }
    let Some(want) = want else {
        close_list(page, (x, y)).await;
        if opts.is_empty() {
            bail!(
                "no list of options opened from {label}; if it is searched as you type, pass `option` with the text to search"
            );
        }
        return Ok(Choice {
            chosen: None,
            options: opts,
        });
    };
    // What showed before typing, for the error when typing filters it all out.
    let offered = opts.clone();
    let mut typed = false;
    if pick_option(&opts, want).is_none()
        && (opts.is_empty() || opts.len() >= PARTIAL_LIST_MIN)
        && focused_is_combobox(page).await
    {
        type_chars(page, want, 0).await?;
        typed = true;
        opts = shown_options(page, css, Some(want), 3_000).await;
    }
    let Some(i) = pick_option(&opts, want) else {
        dispatch_key(page, "Escape").await.ok();
        if typed {
            clear_focused(page).await;
        }
        if opts.is_empty() {
            opts = offered;
        }
        if opts.is_empty() {
            bail!(
                "no list of options opened from {label}{}; if it isn't a dropdown, use click or type",
                if typed {
                    format!(" (searched for {want:?} too)")
                } else {
                    String::new()
                }
            );
        }
        bail!(
            "{label} has no option {want:?}{}; it offers {}",
            if typed { " (searched for it too)" } else { "" },
            option_list(&opts)
        );
    };
    let opt = format!("jev-ref:{OPTION_PICKS}:{i}");
    let at = settled_center(page, &opt).await?;
    let what = format!("the option {:?}", opts[i]);
    check_point(page, at, Some(&opt), &what).await?;
    page.click(Point::new(at.0, at.1))
        .await
        .with_context(|| format!("clicking {what}"))?;
    // A list that picks several (a multi-select) stays open after a pick,
    // covering the fields below it; close it as a person would. A list
    // closing on its own gets a moment first, so its fade isn't taken for
    // open and toggled back.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if options_showing(page).await {
        close_list(page, (x, y)).await;
    }
    Ok(Choice {
        chosen: Some(opts[i].clone()),
        options: opts,
    })
}

/// Close a dropdown opened only to list it: Escape, and when the same
/// options still show (some lists don't listen for it), the opening click
/// again, which toggles them shut.
async fn close_list(page: &Page, at: (f64, f64)) {
    dispatch_key(page, "Escape").await.ok();
    tokio::time::sleep(Duration::from_millis(150)).await;
    if options_showing(page).await {
        page.click(Point::new(at.0, at.1)).await.ok();
    }
}

/// Whether any option of the list [`shown_options`] last read still shows.
async fn options_showing(page: &Page) -> bool {
    iso_eval_function(
        page,
        &format!(
            "function() {{ var o = (globalThis.__bsPicks || {{}})[{key}] || []; \
             return o.some(function(e) {{ if (!e.isConnected) return false; var r = e.getBoundingClientRect(); \
               var w = e.ownerDocument.defaultView || window; \
               return r.width > 0 && r.height > 0 && r.right > 0 && r.left < w.innerWidth && r.bottom > 0 && r.top < w.innerHeight; }}); }}",
            key = js_str(OPTION_PICKS)
        ),
    )
    .await
    .ok()
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
}

/// Empty the focused field if it still holds text, as a person would with
/// select all and delete: a dropdown searched for an option it doesn't
/// have shouldn't keep the search.
async fn clear_focused(page: &Page) {
    let holds = iso_eval_function(
        page,
        "function() { var el = document.activeElement; \
         while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement; \
         return !!el && 'value' in el && String(el.value) !== ''; }",
    )
    .await
    .ok()
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
    if holds {
        dispatch_key(page, "Control+a").await.ok();
        dispatch_key(page, "Backspace").await.ok();
        dispatch_key(page, "Escape").await.ok();
    }
}

/// Where to click to open a dropdown: the element's center, or when the
/// element is off the screen or a sliver (a search input parked off screen
/// or a few pixels wide until the dropdown has focus), the center of the
/// nearest box around it that is on the screen. Returns the point and a
/// reference to the box, for the covered check.
async fn opening_point(page: &Page, css: &str) -> Result<((f64, f64), String)> {
    let decl = format!(
        "function() {{ {deep} var h = __bsDeep({sel}); if (!h) return null; \
         h.el.scrollIntoView({{block:'center',inline:'nearest',behavior:'instant'}}); h = __bsDeep({sel}); \
         var ok = function(r) {{ return r.width >= 4 && r.height >= 4 && r.right > 0 && r.bottom > 0 \
           && r.left < innerWidth && r.top < innerHeight; }}; \
         for (var n = h.el, i = 0; n && n.getBoundingClientRect && i < 4; n = n.parentElement, i++) {{ \
           var r = n.getBoundingClientRect(); \
           if (ok(r)) {{ var store = globalThis.__bsPicks || (globalThis.__bsPicks = {{ order: [] }}); \
             store[{key}] = [n]; \
             return [h.ox + r.left + r.width / 2, h.oy + r.top + r.height / 2]; }} }} \
         return null; }}",
        deep = jev::DEEP_FIND_JS,
        sel = js_str(css),
        key = js_str(OPENER_PICK),
    );
    let v = iso_eval_function(page, &decl).await?;
    match v.as_array().map(|a| (a[0].as_f64(), a[1].as_f64())) {
        Some((Some(x), Some(y))) => Ok(((x, y), format!("jev-ref:{OPENER_PICK}:0"))),
        _ => Ok((element_center(page, css).await?, css.to_string())),
    }
}

/// An element's center once it stops moving: a menu that grows or slides
/// in moves its options for a few hundred milliseconds, and some (such as
/// Material UI's) ignore a click until it has finished. Waits up to a
/// second.
async fn settled_center(page: &Page, css: &str) -> Result<(f64, f64)> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_000);
    let mut last = element_center(page, css).await?;
    loop {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let now = element_center(page, css).await?;
        let still = (now.0 - last.0).abs() < 0.5 && (now.1 - last.1).abs() < 0.5;
        if still || tokio::time::Instant::now() >= deadline {
            return Ok(now);
        }
        last = now;
    }
}

/// Most bytes upload_file attaches.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// A file's type from its name, for the `File` the page receives.
fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("pdf") => "application/pdf",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("rtf") => "application/rtf",
        Some("txt") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("avif") => "image/avif",
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Attach a file to the element at SEL. A file input gets it as its
/// `files`, with input and change events; an element standing for one (a
/// label, a box holding one, or an "Attach" button with one beside it) gets
/// its input's; anything else is treated as a drop zone and receives the
/// drag and drop events a person dropping the file would send.
const UPLOAD_JS: &str = r#"function() { @@DEEP@@ @@DESC@@
  var h = __bsDeep(@@SEL@@); if (!h) return null; var el = h.el;
  var bin = atob(@@B64@@), bytes = new Uint8Array(bin.length);
  for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  var dt = new DataTransfer();
  dt.items.add(new File([bytes], @@NAME@@, { type: @@TYPE@@, lastModified: Date.now() }));
  var isFile = function(n) { return n && n.tagName === 'INPUT' && n.type === 'file'; };
  var input = isFile(el) ? el : null;
  if (!input && el.tagName === 'LABEL' && isFile(el.control)) input = el.control;
  // Pickers often pair a file input with a folder one; files go to the first.
  var FILES = 'input[type=file]:not([webkitdirectory])';
  if (!input && el.querySelector) input = el.querySelector(FILES) || el.querySelector('input[type=file]');
  // A button or label stands for the file input beside it; a box with no
  // input inside is a drop zone, not a way to reach one further up.
  var control = /^(BUTTON|A|LABEL)$/.test(el.tagName) || el.getAttribute('role') === 'button';
  for (var n = el.parentElement, up = 0; control && !input && n && up < 2; n = n.parentElement, up++) {
    var inside = n.querySelectorAll(FILES);
    if (inside.length === 1) input = inside[0];
  }
  if (input) {
    input.files = dt.files;
    input.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    input.dispatchEvent(new Event('change', { bubbles: true, composed: true }));
    return { how: 'input', into: __bsDesc(input), accept: input.getAttribute('accept') || '', count: input.files.length };
  }
  var r = el.getBoundingClientRect(), x = r.left + r.width / 2, y = r.top + r.height / 2;
  ['dragenter', 'dragover', 'drop'].forEach(function(type) {
    el.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, dataTransfer: dt }));
  });
  return { how: 'drop', into: __bsDesc(el), accept: '', count: 1 };
}"#;

/// Whether a file input's `accept` list (empty takes anything) takes
/// `name` of type `mime`.
fn accepts(accept: &str, name: &str, mime: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let list: Vec<String> = accept
        .split(',')
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    list.is_empty()
        || list.iter().any(|a| {
            if let Some(ext) = a.strip_prefix('.') {
                name.ends_with(&format!(".{ext}"))
            } else if let Some(group) = a.strip_suffix("/*") {
                mime.starts_with(&format!("{group}/"))
            } else {
                a == mime
            }
        })
}

/// Go back one entry in history — `act`'s `back` pseudo-action.
async fn go_back(page: &Page) -> Result<()> {
    iso_eval_top(page, "function() { history.back(); return true; }")
        .await
        .map(|_| ())
        .context("going back in history")
}

/// [`jev::FINGERPRINT_FN`] for `act`'s effect check; `None` when the read
/// failed (mid-navigation teardown).
async fn page_fingerprint(page: &Page) -> Option<String> {
    let top = iso_eval_top(page, jev::FINGERPRINT_FN)
        .await
        .ok()?
        .as_str()
        .map(str::to_string)?;
    // An action inside a frame changes the frame, not the top document.
    let Some(frame) = crate::frames::current() else {
        return Some(top);
    };
    let inner = crate::frames::eval(&frame, jev::FINGERPRINT_FN)
        .await
        .ok()?;
    Some(format!("{top}/{}", inner.as_str().unwrap_or_default()))
}

/// Read the page's current url + title for `act`'s final report. `None`
/// means the read itself failed (mid-navigation teardown).
async fn current_url_title(page: &Page) -> Option<(String, String)> {
    let v = iso_eval_top(
        page,
        "function() { return location.href + ' ~~ ' + document.title; }",
    )
    .await
    .ok()?;
    let s = v.as_str()?;
    let (url, title) = s.split_once(" ~~ ")?;
    Some((url.to_string(), title.to_string()))
}

/// Click an element's center through trusted CDP input, naming `label` in
/// errors — the shared path of `act`'s element clicks.
async fn click_label(page: &Page, css: &str, label: &str) -> Result<()> {
    let (x, y) = element_center(page, css).await?;
    check_point(page, (x, y), Some(css), label).await?;
    page.click(Point::new(x, y))
        .await
        .with_context(|| format!("clicking {label}"))?;
    Ok(())
}

/// The `type` tool's result line: what it did and, when the field doesn't
/// read back as typed, what it holds instead. A field left empty, or one
/// that kept none of the text, is an error: the typing didn't take.
fn typed_report(typed: &Typed, text: &str, target: &str, clear: bool) -> Result<String> {
    let n = text.chars().count();
    let Some(holds) = &typed.holds else {
        return Ok(format!("Typed {n} chars into {target}"));
    };
    if typed.set {
        return Ok(format!("Set {target} to {holds:?}"));
    }
    let as_typed = if clear {
        holds == text
    } else {
        holds.ends_with(text)
    };
    if as_typed {
        return Ok(format!("Typed {n} chars into {target}"));
    }
    // Compare letters and digits only, so a mask's spaces and brackets
    // don't count as lost text: "(555) 123-4567" kept all of 5551234567.
    let core = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let kept = core(holds).contains(&core(text));
    if holds.is_empty() || (!kept && !text.trim().is_empty() && core(holds).is_empty()) {
        bail!("typed {n} chars into {target}, but it is empty afterwards: the page dropped them");
    }
    if typed.secret {
        return Ok(format!(
            "Typed {n} chars into {target}; it now holds {} chars",
            holds.chars().count()
        ));
    }
    let shown = clamp_chars(holds, 120);
    if kept {
        Ok(format!(
            "Typed {n} chars into {target}; the page formatted it as {shown:?}"
        ))
    } else {
        Ok(format!(
            "Typed {n} chars into {target}, but it holds {shown:?}: the page changed or dropped some of the text"
        ))
    }
}

/// `s` cut to `max` characters, with an ellipsis when cut.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// What a field holds after [`type_into`]: its text, or `None` when it
/// can't be read (a widget that keeps no value). `secret` for a password
/// field, whose text is never echoed.
struct Typed {
    holds: Option<String>,
    secret: bool,
    /// Set as a value instead of typed: a date, time, range or color input.
    set: bool,
}

/// Optionally clear a field, focus and click it, and type `text` into it:
/// the shared path of the `type` tool and `act`'s value typing. The click
/// is best effort (a field can be focusable but covered); focus is not.
/// Date, time, range and color inputs take no typed text (the browser
/// splits a date into boxes whose order depends on the locale, and a range
/// has no text at all), so their value is set the way the page's own
/// script would, with the events it listens for. Returns what the field
/// holds afterwards, so a caller can tell typing that didn't take.
async fn type_into(
    page: &Page,
    css: &str,
    text: &str,
    clear: bool,
    delay_ms: u64,
) -> Result<Typed> {
    focus_element(page, css)
        .await
        .with_context(|| format!("locating {css}"))?;
    if let Some(typed) = set_special_value(page, css, text).await? {
        return Ok(typed);
    }
    if let Ok((x, y)) = element_center(page, css).await {
        page.click(Point::new(x, y)).await.ok();
    }
    if clear {
        clear_field(page, css).await;
    } else {
        caret_to_end(page, css).await;
    }
    type_chars(page, text, delay_ms)
        .await
        .with_context(|| format!("typing into {css}"))?;
    Ok(field_holds(page, css).await)
}

/// Read what a field holds: an input's or textarea's value, an editable
/// element's text.
async fn field_holds(page: &Page, css: &str) -> Typed {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); if (!h) return null; const el = h.el; \
         const secret = el.type === 'password'; \
         if ('value' in el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) return {{ v: String(el.value), secret: secret }}; \
         if (el.isContentEditable) return {{ v: String(el.innerText || ''), secret: false }}; \
         return null; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
    );
    let v = iso_eval_function(page, &decl).await.unwrap_or(Value::Null);
    Typed {
        holds: v.get("v").and_then(Value::as_str).map(str::to_string),
        secret: v.get("secret").and_then(Value::as_bool).unwrap_or(false),
        set: false,
    }
}

/// Set a date, time, datetime-local, month, week, range or color input to
/// `text` through the input's own value setter (which frameworks like React
/// watch), then fire input and change. Dates also take 03/14/2026 and
/// "March 14, 2026", times 2:30 PM. `None` for any other field; an error
/// when the input refuses the value, naming the format it takes.
async fn set_special_value(page: &Page, css: &str, text: &str) -> Result<Option<Typed>> {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); if (!h) return null; const el = h.el; \
         const kinds = ['date', 'time', 'datetime-local', 'month', 'week', 'range', 'color']; \
         if (el.tagName !== 'INPUT' || kinds.indexOf(el.type) < 0) return null; \
         const t = {text}.trim(), pad = function(n) {{ return String(n).padStart(2, '0'); }}; \
         const ymd = function(d) {{ return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()); }}; \
         const time = function(s) {{ const m = s.match(/^(\\d{{1,2}}):(\\d{{2}})(?::(\\d{{2}}))?\\s*([ap])\\.?m?\\.?$/i); \
           if (!m) return s; let hr = Number(m[1]) % 12; if (/p/i.test(m[4])) hr += 12; \
           return pad(hr) + ':' + m[2] + (m[3] ? ':' + m[3] : ''); }}; \
         const date = function(s) {{ if (/^\\d{{4}}-\\d{{2}}-\\d{{2}}$/.test(s)) return s; \
           const us = s.match(/^(\\d{{1,2}})[\\/.-](\\d{{1,2}})[\\/.-](\\d{{4}})$/); \
           if (us) return us[3] + '-' + pad(us[1]) + '-' + pad(us[2]); \
           const d = new Date(s); return isNaN(d) ? s : ymd(d); }}; \
         let v = t; \
         if (el.type === 'date') v = date(t); \
         else if (el.type === 'time') v = time(t); \
         else if (el.type === 'datetime-local') {{ const m = t.match(/^(.+?)[T ,]+(\\d{{1,2}}:\\d{{2}}.*)$/); if (m) v = date(m[1]) + 'T' + time(m[2]); }} \
         else if (el.type === 'month' && /^\\d{{1,2}}\\/\\d{{4}}$/.test(t)) {{ const p = t.split('/'); v = p[1] + '-' + pad(p[0]); }} \
         const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; \
         set.call(el, v); \
         el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
         el.dispatchEvent(new Event('change', {{ bubbles: true }})); \
         return {{ kind: el.type, v: String(el.value), min: el.min, max: el.max }}; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
        text = serde_json::to_string(text).unwrap_or_else(|_| "''".to_string()),
    );
    let v = iso_eval_function(page, &decl).await?;
    let Some(kind) = v.get("kind").and_then(Value::as_str) else {
        return Ok(None);
    };
    let holds = v
        .get("v")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if holds.is_empty() {
        let format = match kind {
            "date" => "a date such as 2026-03-14 or 03/14/2026",
            "time" => "a time such as 14:30 or 2:30 PM",
            "datetime-local" => "a date and time such as 2026-03-14T14:30",
            "month" => "a month such as 2026-03",
            "week" => "a week such as 2026-W11",
            _ => "a value it accepts",
        };
        bail!("the {kind} input {css} didn't take {text:?}: it wants {format}");
    }
    Ok(Some(Typed {
        holds: Some(holds),
        secret: false,
        set: true,
    }))
}

/// Mark a field `act` typed into, so the rest of the run doesn't offer it
/// again while it holds text (see the extraction's `SKIP_TYPED`).
async fn mark_typed(page: &Page, css: &str) {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); \
         if (h) (globalThis.__bsTyped || (globalThis.__bsTyped = new WeakSet())).add(h.el); \
         return !!h; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
    );
    let _ = iso_eval_function(page, &decl).await;
}

/// Drop the marks an earlier `act` run left, at the start of a new one.
async fn clear_typed_marks(page: &Page) {
    let decl = "function() { globalThis.__bsTyped = new WeakSet(); return true; }";
    let _ = iso_eval_function(page, decl).await;
}

/// Select everything in a field before typing, so the first key replaces
/// it, the way a person clears a field. Setting `value` directly doesn't
/// stick on framework-controlled inputs (React keeps its own state), and
/// typing then appended to the old text. Best effort.
async fn clear_field(page: &Page, css: &str) {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); if (!h) return false; const el = h.el; \
         el.focus(); \
         if (el.isContentEditable) {{ const r = el.ownerDocument.createRange(); r.selectNodeContents(el); \
           const s = el.ownerDocument.getSelection(); s.removeAllRanges(); s.addRange(r); }} \
         else {{ try {{ el.select(); if (el.selectionStart === null) el.value = ''; }} \
           catch (e) {{ el.value = ''; }} }} return true; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
    );
    let _ = iso_eval_function(page, &decl).await;
}

/// Put the caret after a field's text. The click that focuses a field puts
/// it wherever the click landed, often mid-text in a field the page filled
/// itself (a handle made from a title), and typing then splits that text.
/// Best effort.
async fn caret_to_end(page: &Page, css: &str) {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); if (!h) return false; const el = h.el; \
         if (el.isContentEditable) {{ const r = el.ownerDocument.createRange(); r.selectNodeContents(el); r.collapse(false); \
           const s = el.ownerDocument.getSelection(); s.removeAllRanges(); s.addRange(r); }} \
         else {{ try {{ const n = String(el.value || '').length; el.setSelectionRange(n, n); }} catch (e) {{}} }} \
         return true; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
    );
    let _ = iso_eval_function(page, &decl).await;
}

/// Type `text` into whatever has focus. Characters on the US keyboard go in
/// as real key presses, like a person typing; anything else (accents, CJK,
/// emoji) goes in through `Input.insertText`, the way an IME commits it,
/// since there is no key to press for it.
async fn type_chars(page: &Page, text: &str, delay_ms: u64) -> Result<()> {
    for ch in text.chars() {
        let key = ch.to_string();
        if USKEYBOARD_LAYOUT.iter().any(|k| k.key == key) {
            dispatch_key(page, &key).await?;
        } else {
            page.execute(InsertTextParams::new(key))
                .await
                .context("Input.insertText")?;
        }
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Ok(())
}

/// Accepts JavaScript dialogs (alert, confirm, prompt, beforeunload) on a
/// page while `act` runs and records their messages. An open dialog blocks
/// the page's scripts and every CDP call that waits on them, so without this
/// a single `confirm()` hangs the loop. Dropping the guard stops listening.
struct DialogGuard {
    task: tokio::task::JoinHandle<()>,
}

impl DialogGuard {
    async fn watch(page: &Page, seen: Arc<std::sync::Mutex<Vec<String>>>) -> Result<Self> {
        let mut events = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .context("listening for JavaScript dialogs")?;
        let page = page.clone();
        let task = tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                if let Ok(mut seen) = seen.lock() {
                    seen.push(format!("{:?}: {}", ev.r#type, ev.message).to_lowercase());
                }
                let accept = HandleJavaScriptDialogParams::new(true);
                if let Err(e) = page.execute(accept).await {
                    tracing::warn!(error = %e, "accepting a JavaScript dialog failed");
                }
            }
        });
        Ok(Self { task })
    }
}

impl Drop for DialogGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Center of an element in viewport CSS pixels, after scrolling it into view.
/// Finds it through open shadow roots and same-origin iframes too (see
/// [`jev::DEEP_FIND_JS`]). Evaluated in an isolated world so it works
/// regardless of stealth.
async fn element_center(page: &Page, selector: &str) -> Result<(f64, f64)> {
    let decl = format!(
        "function() {{ {deep} let h = __bsDeep({sel}); if (!h) return null; \
         h.el.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}}); h = __bsDeep({sel}); \
         const r = h.el.getBoundingClientRect(); if (!r.width && !r.height) return null; \
         return [h.ox + r.left + r.width/2, h.oy + r.top + r.height/2]; }}",
        deep = jev::DEEP_FIND_JS,
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

/// An element's box in document coordinates (for a screenshot clip), after
/// scrolling it into view; `None` when it has no size.
async fn element_box(page: &Page, selector: &str) -> Result<Option<(f64, f64, f64, f64)>> {
    let decl = format!(
        "function() {{ {deep} let h = __bsDeep({sel}); if (!h) return null; \
         h.el.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}}); h = __bsDeep({sel}); \
         const r = h.el.getBoundingClientRect(); if (!r.width || !r.height) return null; \
         return [h.ox + r.left, h.oy + r.top, r.width, r.height, scrollX, scrollY]; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string())
    );
    let v = iso_eval_function(page, &decl).await?;
    let Some(a) = v.as_array().filter(|a| a.len() == 6) else {
        return Ok(None);
    };
    let n = |i: usize| a[i].as_f64().unwrap_or(0.0);
    // Document coordinates are the top document's, scrolled as it is.
    let (sx, sy) = if crate::frames::current().is_some() {
        let s = iso_eval_top(page, "function() { return [scrollX, scrollY]; }").await?;
        let at = |i: usize| s.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        (at(0), at(1))
    } else {
        (n(4), n(5))
    };
    Ok(Some((n(0) + sx, n(1) + sy, n(2), n(3))))
}

/// A thrown error's message: `text` alone is often just "Uncaught" or
/// "Uncaught (in promise)", and the error itself ("SyntaxError: ... is not a
/// valid selector") is in the exception's description, with its stack.
fn exception_message(exc: &chromiumoxide::cdp::js_protocol::runtime::ExceptionDetails) -> String {
    let thrown = exc.exception.as_ref().and_then(|e| {
        e.description
            .clone()
            .or_else(|| e.value.as_ref().map(|v| v.to_string()))
    });
    match thrown {
        Some(d) => {
            let d: String = d.lines().take(4).collect::<Vec<_>>().join("\n");
            format!("{}: {d}", exc.text.trim_end_matches(':').trim())
        }
        None => exc.text.clone(),
    }
}

/// What is at a screen point, looking into open shadow roots and same-origin
/// iframes, and whether it is `target` (a selector or `jev-ref:`), part of
/// it, or its label. `off` means the point is outside the screen.
#[derive(Debug)]
struct Hit {
    hit: String,
    ok: bool,
    off: bool,
    /// The target is disabled, or ignores clicks along with its parent and
    /// labels, as in a greyed out part of a form.
    dead: bool,
}

const DESCRIBE_JS: &str = r#"var __bsDesc = function(el) {
  if (!el || !el.tagName) return 'nothing';
  var t = el.tagName.toLowerCase();
  if (el.id) t += '#' + el.id;
  else if (typeof el.className === 'string' && el.className.trim()) t += '.' + el.className.trim().split(/\s+/).slice(0, 2).join('.');
  var label = String(el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('placeholder') || '').replace(/\s+/g, ' ').trim();
  if (label) t += ' "' + (label.length > 50 ? label.slice(0, 49) + '...' : label) + '"';
  // Inside a web component (a popup rendered in a shadow root), name its
  // host on the page too: that is what a page script can find and remove.
  var host = el.getRootNode && el.getRootNode().host;
  while (host && host.getRootNode().host) host = host.getRootNode().host;
  if (host) t += ' (inside ' + host.tagName.toLowerCase() + (host.id ? '#' + host.id : '') + ')';
  return t;
};"#;

async fn hit_at(page: &Page, (x, y): (f64, f64), target: Option<&str>) -> Result<Hit> {
    let decl = format!(
        "function() {{ {deep} {desc} var base = globalThis.__bsBase || {{ x: 0, y: 0 }}; \
         var x = {x} - base.x, y = {y} - base.y, sel = {sel}; \
         if (x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return {{ off: true }}; \
         var el = document.elementFromPoint(x, y), ox = 0, oy = 0; \
         for (var i = 0; el && i < 10; i++) {{ \
           if (el.shadowRoot) {{ var inner = el.shadowRoot.elementFromPoint(x - ox, y - oy); \
             if (inner && inner !== el) {{ el = inner; continue; }} }} \
           if (el.tagName === 'IFRAME') {{ var d = null; try {{ d = el.contentDocument; }} catch (e) {{}} \
             if (d) {{ var r = el.getBoundingClientRect(); ox += r.left + el.clientLeft; oy += r.top + el.clientTop; \
               var inside = d.elementFromPoint(x - ox, y - oy); if (inside) {{ el = inside; continue; }} }} }} \
           break; }} \
         var up = function(n) {{ return n.parentNode || n.host || (n.defaultView && n.defaultView.frameElement) || null; }}; \
         var within = function(a, b) {{ for (var n = a; n; n = up(n)) if (n === b) return true; return false; }}; \
         var h = sel ? __bsDeep(sel) : null, target = h && h.el; \
         var bare = el && (el.tagName === 'BODY' || el.tagName === 'HTML'); \
         var ok = !target || !el || within(el, target) || (!bare && within(target, el)); \
         if (!ok) {{ for (var n = el; n; n = up(n)) if (n.tagName === 'LABEL') {{ \
           ok = n.control === target || within(target, n); break; }} }} \
         var pe = function(n) {{ return getComputedStyle(n).pointerEvents === 'none'; }}; \
         var dead = !!target && target.nodeType === 1 && ((target.matches && target.matches(':disabled')) || \
           (pe(target) && target.parentElement && pe(target.parentElement) && !(target.labels && [].some.call(target.labels, function(l) {{ return !pe(l); }})))); \
         return {{ hit: __bsDesc(el), ok: ok, dead: dead }}; }}",
        deep = jev::DEEP_FIND_JS,
        desc = DESCRIBE_JS,
        sel = target.map_or("null".to_string(), |t| serde_json::to_string(t)
            .unwrap_or_else(|_| "null".to_string())),
    );
    let v = iso_eval_function(page, &decl).await?;
    Ok(Hit {
        hit: v
            .get("hit")
            .and_then(Value::as_str)
            .unwrap_or("nothing")
            .to_string(),
        ok: v.get("ok").and_then(Value::as_bool).unwrap_or(true),
        off: v.get("off").and_then(Value::as_bool).unwrap_or(false),
        dead: v.get("dead").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Check that a click or tap at `at` lands on `target` (any element when
/// `None`): a point off the screen, or one where a popup, banner or other
/// overlay covers the target, or a target the page has disabled, would
/// report success while doing nothing, so it fails here instead, naming why. The shared check of
/// `click`, `tap` and `act`. Returns what is at the point.
async fn check_point(
    page: &Page,
    at: (f64, f64),
    target: Option<&str>,
    label: &str,
) -> Result<String> {
    // Inside a frame, the page around it can still cover the point (a
    // cookie banner over an embedded form): it has to reach a frame there.
    if crate::frames::current().is_some() {
        let decl = format!(
            "function() {{ {DESCRIBE_JS} var x = {}, y = {}; \
             if (x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return null; \
             var el = document.elementFromPoint(x, y); \
             for (var i = 0; el && el.shadowRoot && i < 10; i++) {{ var inner = el.shadowRoot.elementFromPoint(x, y); \
               if (!inner || inner === el) break; el = inner; }} \
             return el && el.tagName !== 'IFRAME' && el.tagName !== 'FRAME' ? __bsDesc(el) : null; }}",
            at.0, at.1
        );
        if let Ok(Value::String(cover)) = iso_eval_top(page, &decl).await {
            bail!(
                "{label} is covered by {cover} at ({:.0}, {:.0}), so nothing was clicked; close or remove what covers it (a popup, banner or overlay), or target that element if it is what you meant",
                at.0,
                at.1
            );
        }
    }
    let hit = hit_at(page, at, target).await?;
    if hit.off {
        bail!(
            "({:.0}, {:.0}) is outside the screen, so nothing was clicked; scroll it into view or use a selector",
            at.0,
            at.1
        );
    }
    if hit.dead {
        bail!("{label} can't be clicked: the page has it disabled (greyed out, ignores clicks)");
    }
    if !hit.ok {
        bail!(
            "{label} is covered by {} at ({:.0}, {:.0}), so nothing was clicked; close or remove what covers it (a popup, banner or overlay), or target that element if it is what you meant",
            hit.hit,
            at.0,
            at.1
        );
    }
    Ok(hit.hit)
}

/// Where the page is after an action, for its result: the URL and title,
/// whether it left `before`, the focused element (through shadow roots and
/// same-origin iframes) and an open dialog. Read after a short pause, so a
/// click that navigates or opens something has started to.
#[derive(Serialize, JsonSchema)]
struct AfterAction {
    /// The page's URL after the action.
    url: String,
    /// The page's title after the action.
    title: String,
    /// Whether the URL changed.
    navigated: bool,
    /// The element with keyboard focus, or "nothing".
    focused: String,
    /// An open dialog's text, if one is showing.
    #[serde(skip_serializing_if = "Option::is_none")]
    dialog: Option<String>,
    /// What was at the clicked or tapped point.
    #[serde(skip_serializing_if = "Option::is_none")]
    hit: Option<String>,
    /// With press_key's `repeat`: the focused element after each press.
    #[serde(rename = "focusTrail", skip_serializing_if = "Option::is_none")]
    focus_trail: Option<Vec<String>>,
    /// The page's own dialogs (alert, confirm, prompt, "Leave site?")
    /// answered since the last action, each accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    answered: Vec<String>,
    /// Fields the page rejected during the action, each with its message:
    /// the browser's own form check (a required field left empty), or the
    /// page marking them invalid. The form wasn't sent while any remain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
    /// The options of a dropdown or list open after the action (the one the
    /// focused field controls, else any showing), up to 50: pick one with
    /// select_option or click.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    options: Option<Vec<String>>,
}

const AFTER_ACTION_SETTLE_MS: u64 = 250;

/// Before an action: start noting fields the page rejects during it (see
/// [`rejected_fields`]), and return the URL, for [`after_action`].
async fn before_action(page: &Page) -> Option<String> {
    let arm = "function() { var g = globalThis; g.__bsInvalid = []; \
       if (!g.__bsInvalidOn) { g.__bsInvalidOn = true; \
         document.addEventListener('invalid', function(e) { (g.__bsInvalid || (g.__bsInvalid = [])).push(e.target); }, true); } \
       g.__bsWasInvalid = new Set(Array.from(document.querySelectorAll('[aria-invalid=true]'))); \
       return true; }";
    let _ = iso_eval_function(page, arm).await;
    page_url(page).await
}

/// Fields the page rejected during the action [`before_action`] started:
/// those the browser's own form check failed (a required field left empty,
/// an email without an @), with the message it shows, and those the page's
/// script newly marked aria-invalid, with the error text tied to them. Up
/// to 10.
async fn rejected_fields(page: &Page) -> Vec<String> {
    let decl = format!(
        "function() {{ {desc} var g = globalThis, out = [], seen = new Set(); \
         var text = function(n) {{ return n ? String(n.innerText || n.textContent || '').replace(/\\s+/g, ' ').trim() : ''; }}; \
         var byIds = function(el, attr) {{ var root = el.getRootNode(); \
           return (el.getAttribute(attr) || '').trim().split(/\\s+/).filter(Boolean) \
             .map(function(id) {{ return text(root.getElementById ? root.getElementById(id) : document.getElementById(id)); }}) \
             .filter(Boolean).join(' '); }}; \
         var said = function(el) {{ var m = byIds(el, 'aria-errormessage') || byIds(el, 'aria-describedby'); \
           for (var p = el.parentElement, i = 0; !m && p && i < 3; p = p.parentElement, i++) m = text(p.querySelector('[role=alert]')); \
           return m.slice(0, 160); }}; \
         (g.__bsInvalid || []).forEach(function(el) {{ if (seen.has(el)) return; seen.add(el); \
           out.push(__bsDesc(el) + ': ' + (el.validationMessage || said(el) || 'invalid')); }}); \
         if (g.__bsWasInvalid) Array.from(document.querySelectorAll('[aria-invalid=true]')).forEach(function(el) {{ \
           if (g.__bsWasInvalid.has(el) || seen.has(el)) return; seen.add(el); \
           out.push(__bsDesc(el) + ': ' + (said(el) || 'marked invalid')); }}); \
         g.__bsInvalid = []; g.__bsWasInvalid = null; \
         return out.slice(0, 10); }}",
        desc = DESCRIBE_JS,
    );
    iso_eval_function(page, &decl)
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

async fn page_url(page: &Page) -> Option<String> {
    current_url_title(page).await.map(|(u, _)| u)
}

/// Sets `focused` to the focused element's description, looking through
/// shadow roots and same-origin iframes.
const FOCUSED_JS: &str = "var el = document.activeElement; \
   for (var i = 0; el && i < 10; i++) { \
     if (el.shadowRoot && el.shadowRoot.activeElement) { el = el.shadowRoot.activeElement; continue; } \
     if (el.tagName === 'IFRAME') { var d = null; try { d = el.contentDocument; } catch (e) {} \
       if (d && d.activeElement) { el = d.activeElement; continue; } } \
     break; } \
   var focused = el && el !== document.body && el !== document.documentElement ? __bsDesc(el) : 'nothing';";

/// When focus sits in a frame from another site, which page scripts can't
/// look into, work in that frame for the rest of the call, so the focused
/// field (not the `<iframe>`) is what gets described and pressed into.
async fn follow_focus(page: &Page) {
    if crate::frames::current().is_some() {
        return;
    }
    let into_frame = iso_eval_top(
        page,
        "function() { var el = document.activeElement; \
         while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement; \
         if (!el || el.tagName !== 'IFRAME') return false; \
         try { return !el.contentDocument; } catch (e) { return true; } }",
    )
    .await
    .ok()
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
    if !into_frame {
        return;
    }
    // Frames are listed outer first and an outer frame has focus while one
    // inside it does, so the last that has it is where the focus is.
    let mut focused = None;
    for frame in crate::frames::closed(page).await {
        let has = crate::frames::eval(&frame, "function() { return document.hasFocus(); }")
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if has {
            focused = Some(frame);
        }
    }
    if focused.is_some() {
        crate::frames::set_current(focused);
    }
}

/// The frame whose URL contains `needle`, for evaluate's `frame`.
async fn frame_by_url(page: &Page, needle: &str) -> Result<crate::frames::Frame> {
    let frames = crate::frames::frames(page).await;
    if let Some(f) = frames.iter().find(|f| f.url.contains(needle)) {
        return Ok(f.clone());
    }
    let urls: Vec<String> = frames
        .iter()
        .map(|f| f.url.chars().take(80).collect())
        .collect();
    if urls.is_empty() {
        bail!("no frame's URL contains `{needle}`: the page has no frames");
    }
    bail!(
        "no frame's URL contains `{needle}`; the page's frames: {}",
        urls.join(", ")
    )
}

/// The focused element right now.
async fn focused_now(page: &Page) -> String {
    follow_focus(page).await;
    let decl = format!("function() {{ {DESCRIBE_JS} {FOCUSED_JS} return focused; }}");
    iso_eval_function(page, &decl)
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "nothing".to_string())
}

/// Most presses one press_key call makes.
const MAX_KEY_REPEAT: u64 = 50;

async fn after_action(
    page: &Page,
    before: Option<String>,
    hit: Option<String>,
    (sessions, session_id): (&crate::sessions::SessionManager, &str),
) -> AfterAction {
    tokio::time::sleep(Duration::from_millis(AFTER_ACTION_SETTLE_MS)).await;
    follow_focus(page).await;
    let answered = sessions.take_dialogs(session_id);
    let rejected = rejected_fields(page).await;
    let decl = format!(
        "function() {{ {desc} {focused} \
         var dlg = Array.from(document.querySelectorAll('dialog[open], [role=dialog], [role=alertdialog], [aria-modal=true]')) \
           .find(function(d) {{ var r = d.getBoundingClientRect(), s = getComputedStyle(d); \
             var de = document.documentElement; \
             return r.width > 0 && r.height > 0 && r.right > 0 && r.bottom > 0 && r.left < de.clientWidth && r.top < de.clientHeight \
               && s.visibility !== 'hidden' && s.opacity !== '0'; }}); \
         var text = dlg ? String(dlg.innerText || '').replace(/\\s+/g, ' ').trim().slice(0, 160) : null; \
         var seen = function(o) {{ var r = o.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(o).visibility !== 'hidden'; }}; \
         var list = null; \
         if (el && el.getAttribute && el.getAttribute('aria-expanded') === 'true') {{ \
           var ids = ((el.getAttribute('aria-controls') || '') + ' ' + (el.getAttribute('aria-owns') || '')).trim().split(/\\s+/); \
           for (var k = 0; k < ids.length && !list; k++) {{ var root = el.getRootNode(); \
             list = ids[k] && (root.getElementById ? root.getElementById(ids[k]) : document.getElementById(ids[k])); }} }} \
         var opts = Array.from((list || document).querySelectorAll('[role=option]')).filter(seen).slice(0, 50) \
           .map(function(o) {{ return String(o.innerText || o.textContent || o.getAttribute('aria-label') || '').replace(/\\s+/g, ' ').trim(); }}); \
         return {{ url: location.href, title: document.title, \
           focused: focused, options: opts.length ? opts : null, \
           dialog: dlg ? (text || __bsDesc(dlg)) : null }}; }}",
        desc = DESCRIBE_JS,
        focused = FOCUSED_JS,
    );
    let mut v = iso_eval_top(page, &decl).await.unwrap_or(Value::Null);
    // Inside a frame, focus and an open list are the frame's.
    if let Some(frame) = crate::frames::current() {
        if let Ok(inner) = crate::frames::eval(&frame, &decl).await {
            if let Some(obj) = v.as_object_mut() {
                for key in ["focused", "options"] {
                    if let Some(x) = inner.get(key).filter(|x| !x.is_null()) {
                        obj.insert(key.to_string(), x.clone());
                    }
                }
                if obj.get("dialog").is_none_or(Value::is_null) {
                    if let Some(d) = inner.get("dialog") {
                        obj.insert("dialog".to_string(), d.clone());
                    }
                }
            }
        }
    }
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("(navigating)")
        .to_string();
    AfterAction {
        navigated: before.as_deref().is_some_and(|b| b != url),
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        focused: v
            .get("focused")
            .and_then(Value::as_str)
            .unwrap_or("nothing")
            .to_string(),
        dialog: v.get("dialog").and_then(Value::as_str).map(str::to_string),
        options: v
            .get("options")
            .and_then(|o| serde_json::from_value(o.clone()).ok()),
        url,
        hit,
        focus_trail: None,
        answered,
        rejected,
    }
}

impl AfterAction {
    /// The result line under an action's own: where it left the page.
    fn render(&self, done: &str) -> String {
        let mut s = format!("{done}.");
        if let Some(h) = &self.hit {
            s.push_str(&format!(" It landed on {h}."));
        }
        if self.navigated {
            s.push_str(&format!(
                " The page navigated to {} ({}).",
                self.url, self.title
            ));
        } else {
            s.push_str(&format!(" Still on {} ({}).", self.url, self.title));
        }
        match &self.focus_trail {
            Some(t) => s.push_str(&format!(" Focus after each press: {}.", t.join(" / "))),
            None => s.push_str(&format!(" Focus: {}.", self.focused)),
        }
        if let Some(d) = &self.dialog {
            s.push_str(&format!(" A dialog is open: {d}"));
        }
        for d in &self.answered {
            s.push_str(&format!(" The page asked ({d}) and it was accepted."));
        }
        if !self.rejected.is_empty() {
            s.push_str(&format!(
                " The page rejected {} field{}: {}.",
                self.rejected.len(),
                plural(self.rejected.len()),
                self.rejected.join("; ").trim_end_matches('.')
            ));
        }
        if let Some(o) = &self.options {
            s.push_str(&format!(
                " A list is open with {} option{}: {}.",
                o.len(),
                plural(o.len()),
                option_list(o)
            ));
        }
        s
    }
}

/// Where `act`'s sideways swipe starts: the middle of the in-view part of
/// the largest box on screen that scrolls sideways (a carousel), else the
/// middle of the screen. A carousel below the middle would miss a swipe
/// across it.
async fn swipe_start(page: &Page, direction: &str, vp: Viewport) -> (f64, f64) {
    let mid = (vp.width as f64 / 2.0, vp.height as f64 / 2.0);
    if !matches!(direction, "left" | "right") {
        return mid;
    }
    const FIND: &str = "function() { var best = null, area = 0; \
        var all = document.body ? document.body.getElementsByTagName('*') : []; \
        for (var i = 0; i < all.length && i < 20000; i++) { var el = all[i]; \
          if (el.scrollWidth <= el.clientWidth + 1) continue; \
          if (getComputedStyle(el).overflowX === 'visible') continue; \
          var r = el.getBoundingClientRect(); \
          var w = Math.min(r.right, innerWidth) - Math.max(r.left, 0); \
          var h = Math.min(r.bottom, innerHeight) - Math.max(r.top, 0); \
          if (w > 40 && h > 40 && w * h > area) { area = w * h; \
            best = [Math.max(r.left, 0) + w / 2, Math.max(r.top, 0) + h / 2]; } } \
        return best; }";
    iso_eval_top(page, FIND)
        .await
        .ok()
        .and_then(|v| Some((v.get(0)?.as_f64()?, v.get(1)?.as_f64()?)))
        .unwrap_or(mid)
}

/// Where a swipe from `from` ends: 60% of the screen along `direction`.
fn swipe_end(from: (f64, f64), direction: &str, vp: Viewport) -> (f64, f64) {
    let (dx, dy) = (0.6 * vp.width as f64, 0.6 * vp.height as f64);
    match direction {
        "right" => (from.0 + dx, from.1),
        "up" => (from.0, from.1 - dy),
        "down" => (from.0, from.1 + dy),
        _ => (from.0 - dx, from.1),
    }
}

/// `Input.dispatchTouchEvent` with `touchPoints` always sent. The generated
/// params drop an empty list, and Chrome rejects a touchEnd without the field
/// ("Invalid parameters"), though a lift has no points.
#[derive(Debug, Clone, Serialize)]
struct TouchEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "touchPoints")]
    touch_points: Vec<Value>,
}

impl chromiumoxide::Method for TouchEvent {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "Input.dispatchTouchEvent".into()
    }
}

impl chromiumoxide::Command for TouchEvent {
    type Response = Value;
}

/// One CDP touch event; `point` is the finger's position, `None` on lift.
async fn touch_event(page: &Page, kind: &'static str, point: Option<(f64, f64)>) -> Result<()> {
    let touch_points = point.map_or_else(Vec::new, |(x, y)| vec![json!({ "x": x, "y": y })]);
    page.execute(TouchEvent { kind, touch_points })
        .await
        .context("Input.dispatchTouchEvent")?;
    Ok(())
}

/// Tap a point with a finger: touch down, a moment, lift. The page gets
/// touch and pointer events (`pointerType: "touch"`) and the click a tap
/// makes.
async fn touch_tap(page: &Page, at: (f64, f64)) -> Result<()> {
    touch_event(page, "touchStart", Some(at)).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    touch_event(page, "touchEnd", None).await
}

/// Drag a finger from one point to another over `duration_ms`, in steps of
/// about one frame, then lift: a swipe, or a partial drag when short and
/// slow.
async fn touch_swipe(
    page: &Page,
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
) -> Result<()> {
    let steps = (duration_ms / 16).clamp(5, 120);
    touch_event(page, "touchStart", Some(from)).await?;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let at = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        tokio::time::sleep(Duration::from_millis(duration_ms / steps)).await;
        touch_event(page, "touchMove", Some(at)).await?;
    }
    touch_event(page, "touchEnd", None).await
}

/// One CDP mouse event at a viewport point.
async fn mouse_event(
    page: &Page,
    kind: DispatchMouseEventType,
    (x, y): (f64, f64),
    button: MouseButton,
    buttons: i64,
) -> Result<()> {
    let params = DispatchMouseEventParams::builder()
        .r#type(kind)
        .x(x)
        .y(y)
        .button(button)
        .buttons(buttons)
        .click_count(1)
        .build()
        .map_err(|e| anyhow!("mouse event: {e}"))?;
    page.execute(params)
        .await
        .context("Input.dispatchMouseEvent")?;
    Ok(())
}

/// Right-click a point with trusted input, opening the page's context menu
/// handler (`act`'s right_click).
async fn right_click(page: &Page, at: (f64, f64)) -> Result<()> {
    mouse_event(
        page,
        DispatchMouseEventType::MouseMoved,
        at,
        MouseButton::None,
        0,
    )
    .await?;
    mouse_event(
        page,
        DispatchMouseEventType::MousePressed,
        at,
        MouseButton::Right,
        2,
    )
    .await?;
    mouse_event(
        page,
        DispatchMouseEventType::MouseReleased,
        at,
        MouseButton::Right,
        0,
    )
    .await
}

/// Drag from one point to another with trusted input (`act`'s drag). Mouse
/// down, a few moves and mouse up cover pointer-driven drag libraries; an
/// HTML5 drag-and-drop is intercepted by Chrome and replayed onto the target
/// as drag events, the way Playwright's dragTo does it.
async fn drag(page: &Page, from: (f64, f64), to: (f64, f64)) -> Result<()> {
    page.execute(SetInterceptDragsParams::new(true))
        .await
        .context("Input.setInterceptDrags")?;
    let mut intercepted = page
        .event_listener::<EventDragIntercepted>()
        .await
        .context("listening for drags")?;
    let result = async {
        mouse_event(
            page,
            DispatchMouseEventType::MouseMoved,
            from,
            MouseButton::None,
            0,
        )
        .await?;
        mouse_event(
            page,
            DispatchMouseEventType::MousePressed,
            from,
            MouseButton::Left,
            1,
        )
        .await?;
        for i in 1..=5 {
            let t = f64::from(i) / 5.0;
            let at = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            mouse_event(
                page,
                DispatchMouseEventType::MouseMoved,
                at,
                MouseButton::Left,
                1,
            )
            .await?;
        }
        let html5 = tokio::time::timeout(Duration::from_millis(250), intercepted.next())
            .await
            .ok()
            .flatten();
        if let Some(ev) = html5 {
            for kind in [
                DispatchDragEventType::DragEnter,
                DispatchDragEventType::DragOver,
                DispatchDragEventType::Drop,
            ] {
                page.execute(DispatchDragEventParams::new(
                    kind,
                    to.0,
                    to.1,
                    ev.data.clone(),
                ))
                .await
                .context("Input.dispatchDragEvent")?;
            }
        }
        mouse_event(
            page,
            DispatchMouseEventType::MouseReleased,
            to,
            MouseButton::Left,
            0,
        )
        .await
    }
    .await;
    let _ = page.execute(SetInterceptDragsParams::new(false)).await;
    result
}

/// Focus an element found by [`jev::DEEP_FIND_JS`], for key presses aimed at it.
async fn focus_element(page: &Page, selector: &str) -> Result<()> {
    let decl = format!(
        "function() {{ {deep} const h = __bsDeep({sel}); if (!h) return false; h.el.focus(); return true; }}",
        deep = jev::DEEP_FIND_JS,
        sel = serde_json::to_string(selector).unwrap_or_else(|_| "''".to_string())
    );
    if iso_eval_function(page, &decl).await?.as_bool() != Some(true) {
        bail!("element `{selector}` not found");
    }
    Ok(())
}

/// Press a key at the page level (on whatever's focused) via CDP Input,
/// looking the key up in chromiumoxide's US keyboard layout. Printable keys
/// send `text` with a `keyDown`; named keys (Enter, Tab, Escape, ArrowDown…)
/// use `rawKeyDown`. keyDown + keyUp are dispatched. A combination such as
/// `Shift+Tab` or `Control+a` holds its modifiers down around the key; a
/// lone `+` is still the plus key. `Space` names the space bar.
async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    const MODIFIERS: &[(&str, i64)] = &[("Alt", 1), ("Control", 2), ("Meta", 4), ("Shift", 8)];
    let (mods, key) = match key.rsplit_once('+') {
        Some((mods, k))
            if !k.is_empty()
                && !mods.is_empty()
                && mods
                    .split('+')
                    .all(|m| MODIFIERS.iter().any(|(n, _)| *n == m)) =>
        {
            (mods.split('+').collect::<Vec<_>>(), k)
        }
        _ => (Vec::new(), key),
    };
    let key = if key == "Space" { " " } else { key };
    let def = USKEYBOARD_LAYOUT.iter().find(|k| k.key == key).ok_or_else(|| {
        anyhow!("unknown key `{key}` (use names like Enter, Tab, Escape, ArrowDown, Backspace, Space, a single char, or a combination like Shift+Tab)")
    })?;
    let mask: i64 = mods
        .iter()
        .filter_map(|m| MODIFIERS.iter().find(|(n, _)| n == m).map(|(_, b)| *b))
        .sum();
    let modifier_event = |name: &str, kind: DispatchKeyEventType| {
        let def = USKEYBOARD_LAYOUT.iter().find(|k| k.key == name);
        let mut ev = DispatchKeyEventParams::builder()
            .r#type(kind)
            .key(name)
            .modifiers(mask);
        if let Some(d) = def {
            ev = ev.code(d.code).windows_virtual_key_code(d.key_code);
        }
        ev.build().map_err(|e| anyhow!("modifier key: {e}"))
    };
    for m in &mods {
        page.execute(modifier_event(m, DispatchKeyEventType::RawKeyDown)?)
            .await
            .context("Input.dispatchKeyEvent (modifier down)")?;
    }
    // A modifier other than Shift turns a printable key into a shortcut, which
    // must not also insert its character.
    let shortcut = mask & !8 != 0;
    let text: Option<&str> = if shortcut {
        None
    } else {
        def.text.or(if def.key.chars().count() == 1 {
            Some(def.key)
        } else {
            None
        })
    };
    let down_type = if text.is_some() {
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    let mut down = DispatchKeyEventParams::builder()
        .r#type(down_type)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code)
        .modifiers(mask);
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
        .modifiers(mask)
        .build()
        .map_err(|e| anyhow!("key up: {e}"))?;
    page.execute(up)
        .await
        .context("Input.dispatchKeyEvent (up)")?;
    for m in mods.iter().rev() {
        page.execute(modifier_event(m, DispatchKeyEventType::KeyUp)?)
            .await
            .context("Input.dispatchKeyEvent (modifier up)")?;
    }
    Ok(())
}

async fn wait_for_selector(page: &Page, selector: &str, timeout_ms: u64) -> Result<()> {
    let sel = selector.to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let decl = format!(
        "function() {{ {deep} return !!__bsDeep({css}); }}",
        deep = jev::DEEP_FIND_JS,
        css = serde_json::to_string(&sel).unwrap_or_else(|_| "''".to_string())
    );
    let found = |v: Result<Value>| v.ok().and_then(|v| v.as_bool()) == Some(true);
    loop {
        // The frame this call already works in first: a reference such as
        // an option list's could name something there and, stale, in the
        // top document too. Then the top document, then the frames page
        // scripts can't reach; where it's found, the call works on.
        if let Some(frame) = crate::frames::current() {
            if found(crate::frames::eval(&frame, &decl).await) {
                return Ok(());
            }
        }
        if found(iso_eval_top(page, &decl).await) {
            crate::frames::set_current(None);
            return Ok(());
        }
        // All the frames at once: one at a time, a page of ad frames made
        // each look, and so the whole wait, run long.
        let frames = crate::frames::closed(page).await;
        let hits =
            futures::future::join_all(frames.iter().map(|f| crate::frames::eval(f, &decl))).await;
        if let Some(i) = hits.into_iter().position(found) {
            crate::frames::set_current(frames.into_iter().nth(i));
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for selector {sel}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Requests a tab has in flight, counted from Chrome's network events from
/// when it was made: resource timing only lists a request once it finishes,
/// so it can't tell a page waiting on a slow call from one that's done.
struct InFlight {
    count: Arc<std::sync::atomic::AtomicI64>,
    last_change: Arc<std::sync::Mutex<tokio::time::Instant>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl InFlight {
    async fn watch(page: &Page) -> Result<Self> {
        use chromiumoxide::cdp::browser_protocol::network::{
            EnableParams, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
        };
        page.execute(EnableParams::default())
            .await
            .context("Network.enable")?;
        let count = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let last_change = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
        let bump = |count: Arc<std::sync::atomic::AtomicI64>,
                    last: Arc<std::sync::Mutex<tokio::time::Instant>>,
                    by: i64| {
            move || {
                count.fetch_add(by, std::sync::atomic::Ordering::SeqCst);
                *last.lock().expect("unpoisoned") = tokio::time::Instant::now();
            }
        };
        let mut tasks = Vec::new();
        let mut sent = page.event_listener::<EventRequestWillBeSent>().await?;
        let on = bump(count.clone(), last_change.clone(), 1);
        tasks.push(tokio::spawn(async move {
            while sent.next().await.is_some() {
                on();
            }
        }));
        let mut done = page.event_listener::<EventLoadingFinished>().await?;
        let on = bump(count.clone(), last_change.clone(), -1);
        tasks.push(tokio::spawn(async move {
            while done.next().await.is_some() {
                on();
            }
        }));
        let mut failed = page.event_listener::<EventLoadingFailed>().await?;
        let on = bump(count.clone(), last_change.clone(), -1);
        tasks.push(tokio::spawn(async move {
            while failed.next().await.is_some() {
                on();
            }
        }));
        Ok(Self {
            count,
            last_change,
            tasks,
        })
    }

    /// Wait until nothing has been in flight for [`NETWORK_QUIET_MS`], up to
    /// `timeout_ms`. False if it never went quiet.
    async fn idle(&self, timeout_ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let now = tokio::time::Instant::now();
            let quiet = now - *self.last_change.lock().expect("unpoisoned");
            if self.count.load(std::sync::atomic::Ordering::SeqCst) <= 0
                && quiet >= Duration::from_millis(NETWORK_QUIET_MS)
            {
                return true;
            }
            if now >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// How long without a new request counts as the network gone quiet.
const NETWORK_QUIET_MS: u64 = 500;

/// The longest plain pause `wait_for` takes, under the per-call time limit
/// hosts like Executor put on a tool call.
const MAX_PAUSE_MS: u64 = 25_000;

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
                "deviceScaleFactor": {
                    "type": "number",
                    "minimum": 0.5,
                    "maximum": 4,
                    "description": "Device pixels per CSS pixel (window.devicePixelRatio), e.g. 2.625 for a Pixel 7. Default 1."
                },
            }),
        ),
        ("required", json!(["width", "height"])),
        ("additionalProperties", Value::Bool(false)),
    ]);
    let str_list_t = obj(&[
        ("type", Value::String("array".into())),
        ("items", str_t.clone()),
        ("minItems", json!(2)),
        ("maxItems", json!(255)),
    ]);
    let values_t = obj(&[
        ("type", Value::String("object".into())),
        ("additionalProperties", str_t.clone()),
    ]);
    let network_t = json!({
        "oneOf": [
            { "type": "string", "enum": ["off", "offline", "slow3g", "slow4g", "fast4g"] },
            {
                "type": "object",
                "properties": {
                    "latencyMs": { "type": "number", "minimum": 0 },
                    "downloadKbps": { "type": "number", "exclusiveMinimum": 0 },
                    "uploadKbps": { "type": "number", "exclusiveMinimum": 0 },
                },
                "required": ["latencyMs", "downloadKbps", "uploadKbps"],
                "additionalProperties": false,
            },
        ],
    });
    let session_id = (
        "sessionId",
        &str_t,
        "The id returned by open_browser_session. Errors if no open session has it.",
    );
    let tab_index = (
        "tabIndex",
        &nonneg_int_t,
        "A tab's 0-based index from list_pages. Errors if the session has no tab at that index.",
    );
    let state_name = (
        "name",
        &str_t,
        "Name of the saved state: letters, digits, '.', '_' and '-' only.",
    );
    vec![
        tool(
            "open_browser_session",
            "Open a new isolated browser session and return its sessionId; call this first, then pass the id to every other tool. Each session is its own Chrome BrowserContext with separate cookies, storage and tabs, so sessions never see each other, and it starts with one blank tab. Don't open a new session per page: reuse one and use new_page for more tabs. To start logged in, follow with load_browser_state before navigate. Set `viewport` for a specific window size and `useMobileUA` to be a phone: a mobile User-Agent and client hints, mobile device metrics (the page's viewport meta applies) and a touch screen (navigator.maxTouchPoints, touch events, pointer: coarse), so sites pick their touch behavior; use tap and swipe on it, and act taps and swipes on its own. Both are fixed for the session's life. Give a `name` (e.g. `github`) to keep a login across tasks: opening that name again reuses the session while it is open, and otherwise opens a new one with the login saved under the name, which is saved when a takeover finishes and when the session closes. Sessions persist until close_browser_session, or until the reaper role closes ones idle for MAX_IDLE_HOURS (default 24). Fails if Chrome is unreachable at BROWSER_URL.",
            object_schema(
                &[
                    (
                        "viewport",
                        &viewport_t,
                        "Window size in CSS pixels, { width, height, deviceScaleFactor? }. Default 1280x800 at 1x.",
                    ),
                    (
                        "useMobileUA",
                        &bool_t,
                        "Emulate a phone: a mobile User-Agent and client hints, mobile device metrics and a touch screen. Pair it with a phone-sized viewport such as 390x844. Default false.",
                    ),
                    (
                        "device",
                        &obj(&[
                            ("type", Value::String("string".into())),
                            (
                                "enum",
                                json!(
                                    crate::devices::DEVICES
                                        .iter()
                                        .map(|d| d.name)
                                        .collect::<Vec<_>>()
                                ),
                            ),
                        ]),
                        "A device preset: its screen size, pixel ratio, User-Agent and, for phones and tablets, a touch screen. `viewport`, `useMobileUA` and `userAgent` override parts of it.",
                    ),
                    (
                        "userAgent",
                        &str_t,
                        "A User-Agent string to send instead of Chrome's, e.g. a specific phone's; with useMobileUA the session is also a touch phone. A non-Chrome one is sent without Chrome's client hints.",
                    ),
                    (
                        "name",
                        &str_t,
                        "Reuse the open session with this name, or open one with the login saved under it; its login is saved under the name on takeover and close.",
                    ),
                ],
                &[],
            ),
            writes(false, false),
        ),
        tool(
            "close_browser_session",
            "Close a browser session for good: closes all its tabs, discards its cookies and storage, and deletes its console and network logs and saved .cpuprofile files. Call it when the task is done so the session doesn't sit open until the reaper closes it, but leave a session open while its login may be needed again soon. Use close_tab instead to close one tab and keep the session. A session opened with a `name` saves its login under the name first; otherwise save_browser_state first if the login should outlive the session. Idempotent: closing an unknown or already-closed session succeeds, so it is safe in cleanup code. Returns a confirmation.",
            object_schema(
                &[(
                    "sessionId",
                    &str_t,
                    "The id returned by open_browser_session. An unknown or already-closed id succeeds without error.",
                )],
                &["sessionId"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "list_browser_sessions",
            "List every open browser session on the shared Chrome, sorted by sessionId, including ones opened by other agents; it takes no arguments. Use it to recover a lost sessionId or to find sessions left open; don't use it to see the tabs inside one session (list_pages) or saved logins (list_browser_states). activeUrl is null for a session with no tabs. The list is empty if none are open.",
            empty_obj_schema(),
            read_only(),
        ),
        tool(
            "new_page",
            "Open a new tab in the session and make it the active tab, so later navigate/click/type/screenshot calls act on it. Use navigate instead to load a URL in the current tab, and switch_tab to go back to an existing one. Tabs stay open until close_tab or close_browser_session, so close ones you no longer need. With `url`, the tab loads it and the call returns after the load event; if the URL fails to load, the call errors but the new tab still exists and is active. The returned index is the new tab's list_pages index.",
            object_schema(
                &[
                    session_id,
                    (
                        "url",
                        &url_t,
                        "Absolute URL to load in the new tab. Omit for about:blank.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(false, true),
        ),
        tool(
            "list_pages",
            "List the session's tabs in a stable order; it changes nothing. The active tab (the one navigate/evaluate/screenshot/click target by default) is marked with `*` in the text and `active: true` in the structured output. Call it before switch_tab, close_tab, or evaluate/profiling with `tabIndex`, to get a current index; use list_browser_sessions to see sessions rather than tabs, and list_visits for navigation history. Indices stay stable until a tab is closed, after which later tabs shift down one. The list is empty if every tab was closed; the next navigate opens a new one.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "switch_tab",
            "Make the tab at `tabIndex` the active tab, so later navigate/evaluate/screenshot/click/type calls target it, and bring it to the front in Chrome (the human-takeover live view follows it too). Use new_page to open a tab instead, and pass `tabIndex` straight to evaluate or the profiling tools to act on another tab without switching. Indices come from list_pages and shift when a tab closes, so list again if unsure. Fails if the session has no tab at that index.",
            object_schema(&[session_id, tab_index], &["sessionId", "tabIndex"]),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "close_tab",
            "Close the tab at `tabIndex` (from list_pages). The session and its other tabs stay open; if the closed tab was active, the last remaining tab becomes active. Tabs after it shift down one index, so call list_pages again before using another index. Closing the last tab leaves the session open with no tabs; the next navigate opens a new one. To end the whole session, use close_browser_session. Returns a confirmation. Fails if the session has no tab at that index.",
            object_schema(&[session_id, tab_index], &["sessionId", "tabIndex"]),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(false)
                .open_world(false),
        ),
        tool(
            "navigate",
            "Load a URL in the session's active tab and return once the page's load event fires, or with `waitUntil: networkidle` once its requests go quiet, or once `text` shows. Use new_page instead to keep the current page and load the URL in a new tab. If every tab was closed, a new one is opened. `url` must be absolute (https://, http://, data:, about:); `timeout` bounds only the wait for the load event. The returned url is the final one after any redirects. Fails with Chrome's net error (e.g. net::ERR_NAME_NOT_RESOLVED) if the page can't load, or if `timeout` passes first, in which case the page may still be loading.",
            object_schema(
                &[
                    session_id,
                    ("url", &url_t, "Absolute URL to load, including the scheme."),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for the load event before failing. Default: no limit.",
                    ),
                    (
                        "waitUntil",
                        &obj(&[
                            ("type", Value::String("string".into())),
                            ("enum", json!(["load", "networkidle"])),
                        ]),
                        "`load` (default) returns at the load event; `networkidle` also waits until no new request has started for 500ms, for pages that render themselves after loading.",
                    ),
                    (
                        "text",
                        &str_t,
                        "Also wait until this text shows in the page (case-sensitive), failing if it doesn't.",
                    ),
                    (
                        "settleTimeout",
                        &pos_int_t,
                        "Milliseconds to wait for `waitUntil: networkidle` or `text` after the load event, at most 25000. Default 10000.",
                    ),
                ],
                &["sessionId", "url"],
            ),
            writes(false, true),
        ),
        tool(
            "take_screenshot",
            "Capture a PNG of the session's active tab, or one element of it, and return it as an image or, with `save`, as a file path. Use it when visual layout, styling or images matter; use take_snapshot for a cheaper text view of the page's structure, and evaluate to read specific values. `fullPage` captures the whole scrollable page instead of the visible viewport, which can make a very large image on long pages. It reads the page without changing it. Returns the image plus a caption with its size in bytes.",
            object_schema(
                &[
                    session_id,
                    (
                        "fullPage",
                        &bool_t,
                        "Capture the whole scrollable page instead of only the visible viewport. Default false.",
                    ),
                    (
                        "selector",
                        &str_t,
                        "Capture only this element (CSS or `jev:` plain language), scrolled into view.",
                    ),
                    (
                        "scale",
                        &num_t,
                        "Resize the image by this factor, 0.05 to 4 (e.g. 0.5 for a smaller file). Default 1.",
                    ),
                    (
                        "save",
                        &bool_t,
                        "Save the PNG to a file on the host (under $STATE_DIR/screenshots/, kept after the session closes) and return { path, bytes } instead of the image, for callers that can't take an image (a sandbox) or want the file. Default false.",
                    ),
                ],
                &["sessionId"],
            ),
            read_only(),
        ),
        tool(
            "measure_page",
            "Measure how fast a page loads, like a small Lighthouse performance run, in the session as it is: its login, device and throttling (set_emulation). Reloads the active tab (or loads `url`), waits for its requests to go quiet, and returns server response time, first and largest contentful paint (and the element it was), cumulative layout shift, total blocking time from long tasks, DOMContentLoaded and load times, requests and bytes by type and host, and the largest resources, each metric rated good, needs improvement or poor by the Core Web Vitals and Lighthouse thresholds. Use start_profiling instead to find which JavaScript is slow. Reloading loses unsaved page state.",
            object_schema(
                &[
                    session_id,
                    (
                        "url",
                        &url_t,
                        "Load this URL instead of reloading the current page.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for the load and for its requests to go quiet, at most 25000. Default 20000.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(false, true),
        ),
        tool(
            "take_snapshot",
            "Return the accessibility tree of the session's active tab as indented text: each node's role and name (links, buttons, headings, form fields and their labels, and current values), nested by structure. Use it to find what's on the page and what to click or type into, far more cheaply than take_screenshot; use take_screenshot when visual layout matters, and evaluate for data not shown as text. It reads the page without changing it and returns the whole tree, so a large page gives long output.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "act",
            "Act toward a goal on the session's active tab in one call: the server extracts the page's interactive elements (including open shadow roots and same-origin iframes) and visible text, asks the Jev decision model for the next step, does it (click, type, select, hover, right-click, drag, scroll, back, wait, or press a key such as Enter, Escape, Tab or an arrow; on a mobile session it taps instead of clicking, drags with a finger, and can swipe an element or the screen), and repeats until the goal reads achieved, nothing looks able to make progress, or the step budget runs out. Each step is one cheap Jev request (~150 to 500 ms), not a driving-LLM round trip, so multi-step flows take seconds and fractions of a cent. Pass `values` (e.g. { query: \"Oolong\" }) for anything to type: Jev only sees the keys and picks which field each belongs to; the text goes straight to the page, so secrets never reach a model. JavaScript dialogs are accepted and listed in `dialogs`, and a link that opens a new tab is followed there. Needs TYPESAFE_API_KEY; every step lands on an element the extraction just offered, and when Jev isn't sure of a step it stops with status `uncertain` and the `candidates` it was weighing instead of guessing. `status` is completed, likely_completed (Jev leaned toward done but wasn't sure), uncertain, stuck, needs_login (pair with request_human_takeover or load_browser_state; a run with `values` keeps going at a login form), needs_human (a CAPTCHA or bot check; use request_human_takeover), needs_confirmation (only with confirmIrreversible; `pending` names the click it held back), blocked (an error page), no_progress or max_steps. Returns a per-step trace with confidences. Jev can be eager to call a goal met: verify with take_snapshot or take_screenshot afterwards. For a single action, click/type with a plain-language selector is cheaper.",
            object_schema(
                &[
                    session_id,
                    (
                        "goal",
                        &str_t,
                        "What to achieve on the active tab, e.g. \"open the article about espresso\". Re-decided after every step against the page as it is.",
                    ),
                    (
                        "values",
                        &values_t,
                        "Text the loop may type, keyed by role, e.g. { query: \"Oolong\", password: \"hunter2\" }. Only the keys are sent to Jev; the values are typed via CDP.",
                    ),
                    (
                        "maxSteps",
                        &pos_int_t,
                        "Step budget before stopping with status max_steps. Default 8, capped at 32.",
                    ),
                    (
                        "confirmIrreversible",
                        &bool_t,
                        "Stop with status needs_confirmation before a click Jev judges can't be undone (placing an order, paying, sending, deleting). Default false.",
                    ),
                ],
                &["sessionId", "goal"],
            ),
            writes(true, true),
        ),
        tool(
            "ask",
            "Ask the Jev decision model a question about the session's active tab and get a calibrated answer in one cheap call (~150 to 500 ms, a fraction of a cent), judged on the page's URL, title and visible text (an open dialog's first; up to 20,000 characters). Without `options` it is a yes/no question, e.g. \"Is an error message showing?\" or \"Is the user signed in?\", answered with the probability of yes. With `options` it picks the one that fits, e.g. question \"Which page is this?\" with [\"product page\", \"cart\", \"checkout\", \"error page\"], with each option's probability. Useful for branching a script without a large model, and for checking a result after act. Jev reads and judges; it is weak at counting, arithmetic and dates, so ask those of evaluate or take_snapshot instead. Needs TYPESAFE_API_KEY; any Jev problem is an error, never a default answer. It reads the page without changing it.",
            object_schema(
                &[
                    session_id,
                    (
                        "question",
                        &str_t,
                        "The question, about the page as it is now. Phrase a yes/no question as a statement or a question with a clear yes, e.g. \"The order was placed successfully.\"",
                    ),
                    (
                        "options",
                        &str_list_t,
                        "Answers to choose between (2 to 255). Leave out for a yes/no question.",
                    ),
                ],
                &["sessionId", "question"],
            ),
            ToolAnnotations::new().read_only(true).open_world(true),
        ),
        tool(
            "read",
            "Read only the parts of the session's active tab that help answer a question, instead of the whole page: the page's visible text (an open dialog's, when one is showing) is split into passages of about 600 characters under their headings, the Jev decision model rates each one's relevance in parallel requests, and the relevant ones come back in page order under `## heading` lines, up to `maxChars`. A long article shrinks to the few sections that matter, saving the agent's context; a page under 8,000 characters comes back whole without asking Jev. Use take_snapshot for the page's structure and controls, and read for its content. Needs TYPESAFE_API_KEY; any Jev problem is an error. It reads the page without changing it.",
            object_schema(
                &[
                    session_id,
                    (
                        "question",
                        &str_t,
                        "What you want to know from the page, e.g. \"When and why did Japan surrender?\" or \"the refund policy\".",
                    ),
                    (
                        "maxChars",
                        &pos_int_t,
                        "Most characters of passages to return, the most relevant first if they don't all fit. Default 12000.",
                    ),
                ],
                &["sessionId", "question"],
            ),
            ToolAnnotations::new().read_only(true).open_world(true),
        ),
        tool(
            "click",
            "Click in the session's active tab with real CDP mouse input (trusted events — fires handlers reliably and grants user activation, so it can open popups/new tabs that a JS click can't). Pass a CSS `selector` to click the first match's center, after waiting up to `timeout` for it and scrolling it into view. Plain language works too: a `jev:` prefix (e.g. `jev: the login button`) goes straight to the Jev decision model, and with TYPESAFE_API_KEY configured on the server any selector that matches nothing as CSS is retried as a plain-language goal the same way — the server sends the page's interactive elements to Jev, which picks one (on low confidence or any Jev problem it errors instead of guessing, so nothing is clicked blindly; elements are picked from the page as it is at call time, so wait_for content first if the page is still loading). Or pass both `x` and `y` to click viewport coordinates, which are used only when there is no selector. Don't use evaluate to click: synthetic JS clicks can be ignored by pages. Coordinates outside the viewport are not rejected; the click lands on nothing. A click can submit forms or trigger purchases, like a real user's. Returns a confirmation naming what was clicked, and `rejected`: fields the page refused during the click with their messages (a required field left empty, or the page's own error text), when a form wasn't sent. Fails if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it, or if neither a selector nor both coordinates are given.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "CSS selector of the element to click; the first match is used. Or describe the element in plain words — `jev:<goal>` (e.g. `jev: the login button`) goes straight to the Jev decision model, and when TYPESAFE_API_KEY is set a selector that matches nothing as CSS is retried as such a description.",
                    ),
                    (
                        "x",
                        &num_t,
                        "Horizontal viewport coordinate in CSS pixels, used with `y` when no selector is given.",
                    ),
                    (
                        "y",
                        &num_t,
                        "Vertical viewport coordinate in CSS pixels, used with `x` when no selector is given.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(true, true),
        ),
        tool(
            "tap",
            "Tap in the session's active tab with a finger: real CDP touch input, so the page gets touch events, pointer events with pointerType \"touch\" and the click a tap makes. Use it on mobile sessions (open_browser_session with useMobileUA, which also makes the page see a touch screen) for anything a phone user would tap; click sends mouse input instead. Pass a CSS `selector` (or plain words, resolved by Jev as for click) to tap the first match's center after waiting up to `timeout` for it, or both `x` and `y` for viewport coordinates. A tap can submit forms or trigger purchases, like a real user's. Returns a confirmation naming what was tapped.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "CSS selector of the element to tap; the first match is used. Or describe it in plain words, as for click.",
                    ),
                    (
                        "x",
                        &num_t,
                        "Horizontal viewport coordinate in CSS pixels, used with `y` when no selector is given.",
                    ),
                    (
                        "y",
                        &num_t,
                        "Vertical viewport coordinate in CSS pixels, used with `x` when no selector is given.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(true, true),
        ),
        tool(
            "swipe",
            "Swipe a finger across the session's active tab with real CDP touch input (touchstart, touchmove, touchend, and the matching pointer events), e.g. to close a drawer, move a carousel, pull to refresh or scroll the way a phone does. Start on an element (`selector`, or plain words as for click), at `x`,`y`, or by default the middle of the screen; end `distance` pixels away in `direction` (default 60% of the screen along that axis) or exactly at `toX`,`toY`. `durationMs` sets the speed (default 300): short and slow drags a drawer partway, to check it snaps back. Use it on mobile sessions (open_browser_session with useMobileUA); pages that listen only for mouse events won't react. Returns where the swipe started and ended.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "Element to start on (its center): a CSS selector, or plain words as for click.",
                    ),
                    (
                        "x",
                        &num_t,
                        "Start, horizontal viewport coordinate, used with `y` when no selector is given.",
                    ),
                    (
                        "y",
                        &num_t,
                        "Start, vertical viewport coordinate, used with `x` when no selector is given.",
                    ),
                    (
                        "direction",
                        &obj(&[
                            ("type", Value::String("string".into())),
                            ("enum", json!(["left", "right", "up", "down"])),
                        ]),
                        "Which way the finger moves. Needed unless `toX` and `toY` are given.",
                    ),
                    (
                        "distance",
                        &num_t,
                        "How far to move in `direction`, in CSS pixels. Default 60% of the screen's width (left, right) or height (up, down).",
                    ),
                    (
                        "toX",
                        &num_t,
                        "End, horizontal viewport coordinate, with `toY`; overrides `direction`.",
                    ),
                    (
                        "toY",
                        &num_t,
                        "End, vertical viewport coordinate, with `toX`; overrides `direction`.",
                    ),
                    (
                        "durationMs",
                        &pos_int_t,
                        "How long the finger takes to move, in milliseconds. Default 300.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(true, true),
        ),
        tool(
            "type",
            "Type text into the first element matching a CSS selector in the session's active tab: waits up to `timeout` for it, clicks it to focus, then sends real key events for each character, so the page's input handlers fire. Plain language works too: a `jev:` prefix (e.g. `jev: the email field`) goes straight to the Jev decision model, and with TYPESAFE_API_KEY set on the server a selector that matches nothing as CSS is retried as a plain-language goal — Jev picks the input from the page's interactive elements (it errors rather than guesses); the text itself always comes from you. Use press_key afterwards for Enter or Tab, and press_key alone for single keys. `clear` empties the field first; without it, text is appended. `delay` paces the characters for pages that react to typing speed. A date, time, datetime-local, month, week, range or color input takes no typed text, so its value is set directly with the events the page listens for: dates as 2026-03-14 or 03/14/2026, times as 14:30 or 2:30 PM, a range as a number. Afterwards it reads the field back: it reports text the page reformatted (a phone mask), and fails when the field is empty afterwards (the page dropped the text). Target an input, textarea or contenteditable element; typing into a button reports success but changes nothing. Returns how many characters were typed, or the value set. Fails if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "CSS selector of the input, textarea or editable element. Or describe it in plain words — `jev:<goal>` (e.g. `jev: the email field`) goes straight to the Jev decision model, and when TYPESAFE_API_KEY is set a selector that matches nothing as CSS is retried as such a description.",
                    ),
                    ("text", &str_t, "The text to type."),
                    (
                        "delay",
                        &nonneg_int_t,
                        "Milliseconds to pause after each character, for pages that react to typing speed. Default 0.",
                    ),
                    (
                        "clear",
                        &bool_t,
                        "Empty the field's current value before typing. Default false, which appends.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId", "selector", "text"],
            ),
            writes(true, true),
        ),
        tool(
            "select_option",
            "Choose an option in a dropdown or a radio group by its text, in the session's active tab. Works on a native `<select>` (set directly, firing input and change), on a radio group (pass a radio, its fieldset or a `role=radiogroup`; the option is clicked as a person would and checked to be selected) and on custom dropdowns drawn by scripts (a combobox, a React or other component select, a searchable list): it opens the dropdown with a real click, types `option` to filter when the option isn't in the list showing (long lists drawn in part, and lists searched as you type), clicks the option, and reads the field back. `option` matches exactly ignoring case, else the shortest option starting with it, else the shortest containing it. `selector` is the dropdown: the select, the combobox input or the box you'd click to open it (CSS, or plain words as for click). Without `option`, opens the dropdown, returns its `options` and closes it, choosing and typing nothing: look before you pick. It types only when the list may be partial (empty, loading, or 20 options or more), and clears what it typed when nothing matches. Returns what was chosen, the options offered, what the field shows after and whether that is the option (`confirmed`; a field can show a short form, such as \"+1\" for a country), with where the page is as for click. Fails listing the options offered when none matches, or when no list opens.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "The dropdown: a `<select>` (or its label), the combobox input, or the box you'd click to open it. CSS, or plain words as for click.",
                    ),
                    (
                        "option",
                        &str_t,
                        "The option's text, or the start of it (\"United States\" picks \"United States +1\"). Leave it out to list the options without choosing.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId", "selector"],
            ),
            writes(false, true),
        ),
        tool(
            "upload_file",
            "Attach a file in the session's active tab, as if picked in the file dialog (which can't be driven): the page gets a real File with input and change events. `selector` is the file input, or what stands for it on the page: an \"Attach\" or \"Upload\" button, its label or the box around it, whose hidden file input is used. An element with no file input behind it is treated as a drop zone and receives the drag and drop a person dropping the file would make. Give the file as a `path` the server can read, or as `content` (base64) with its `name`. Prefer `path`: `content` passes the whole file through your context.@@UPLOADS@@ Up to 25 MB. Returns what received the file and whether the page then shows its name, with where the page is as for click; warns when the input's `accept` list doesn't take the file.",
            object_schema(
                &[
                    session_id,
                    (
                        "selector",
                        &str_t,
                        "The file input, an Attach/Upload button or label for it, or a drop zone. CSS, or plain words as for click.",
                    ),
                    (
                        "content",
                        &str_t,
                        "The file's bytes as base64. Needs `name`.",
                    ),
                    (
                        "name",
                        &str_t,
                        "The file name the page sees, such as resume.pdf. Defaults to the name in `path`.",
                    ),
                    (
                        "path",
                        &str_t,
                        "A file on the machine the server runs on, instead of `content`.",
                    ),
                    (
                        "mimeType",
                        &str_t,
                        "The file's type. Default from the name's extension (application/pdf for .pdf).",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                ],
                &["sessionId", "selector"],
            ),
            writes(false, true),
        ),
        tool(
            "press_key",
            "Press a key via CDP keyboard input in the session's active tab. `key` is a name (Enter, Tab, Escape, Backspace, ArrowDown, Space, …), a single character, or a combination held down together such as `Shift+Tab` or `Control+a` (modifiers: Shift, Control, Alt, Meta). With a `selector` (CSS; or plain language — a `jev:` goal such as `jev: the password field` goes straight to the Jev decision model, and with TYPESAFE_API_KEY set a selector that matches nothing as CSS is retried as plain language), waits up to `timeout` for it and focuses it first (e.g. type into a field then press Enter); without, presses on whatever is focused, and `timeout` is unused. Use type for whole strings and click to activate buttons. Pressing Enter can submit a form, like a real user. Returns a confirmation. Fails on an unknown key name, or if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it.",
            object_schema(
                &[
                    session_id,
                    (
                        "key",
                        &str_t,
                        "Key name in the US layout (Enter, Tab, Escape, Backspace, ArrowDown, Space…), one character, or a combination such as Shift+Tab or Control+a.",
                    ),
                    (
                        "selector",
                        &str_t,
                        "CSS selector of an element to focus before pressing. Or describe it in plain words (`jev:<goal>` goes straight to the Jev decision model; when TYPESAFE_API_KEY is set a selector that matches nothing as CSS is retried as such a description). Omit to press on the focused element.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for `selector` to appear. Default 5000.",
                    ),
                    (
                        "repeat",
                        &pos_int_t,
                        "Press it this many times (at most 50), e.g. to walk focus with Tab; the result's `focusTrail` lists the focused element after each press. Default 1.",
                    ),
                ],
                &["sessionId", "key"],
            ),
            writes(true, true),
        ),
        tool(
            "scroll",
            "Scroll the session's active tab with a real mouse-wheel event at a viewport position; whatever scrollable element is under (`x`,`y`) scrolls, so point inside a scrollable panel to scroll it rather than the page. `deltaY` and `deltaX` are in pixels; negative values scroll up or left. Useful to reveal lazy-loaded content and to generate human-like behavioral signals. Don't use it just to reach an element before clicking: click scrolls its target into view itself. Returns immediately without waiting for content to load; follow with wait_for if needed.",
            object_schema(
                &[
                    session_id,
                    (
                        "deltaY",
                        &num_t,
                        "Vertical pixels to scroll; negative scrolls up. Default 600.",
                    ),
                    (
                        "deltaX",
                        &num_t,
                        "Horizontal pixels to scroll; negative scrolls left. Default 0.",
                    ),
                    (
                        "x",
                        &num_t,
                        "Wheel position from the viewport's left edge in CSS pixels. Default 50.",
                    ),
                    (
                        "y",
                        &num_t,
                        "Wheel position from the viewport's top edge in CSS pixels. Default 50.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(false, true),
        ),
        tool(
            "move_mouse",
            "Move the mouse to viewport coordinates `x`,`y` (CSS pixels from the top-left) in the session's active tab via CDP input (trusted move event), so the page's hover, mouseover and mousemove handlers fire. Use it to open hover menus and tooltips before reading or clicking them, and to add human-like behavioral telemetry. Don't use it to click, drag or scroll: use click and scroll. Coordinates outside the viewport are not rejected; the move just hovers nothing. Returns a confirmation with the position.",
            object_schema(
                &[
                    session_id,
                    (
                        "x",
                        &num_t,
                        "Horizontal position from the viewport's left edge in CSS pixels.",
                    ),
                    (
                        "y",
                        &num_t,
                        "Vertical position from the viewport's top edge in CSS pixels.",
                    ),
                ],
                &["sessionId", "x", "y"],
            ),
            writes(false, true),
        ),
        tool(
            "wait_for",
            "Wait until a CSS selector matches an element, a text string appears in the page's visible text, or a JavaScript `expression` is truthy, in the session's active tab, polling every 100ms; or, with only `ms`, just pause. Use it after navigate, click or scroll when content loads asynchronously. Pass one of `selector`, `text` or `expression`; `selector` wins if several are given. `ms` alone is a plain pause for hosts with no timer of their own. `text` is a case-sensitive substring of document.body.innerText, so text hidden with CSS doesn't count; it is looked for in frames from other sites too. It reads the page without changing it. Returns which selector or text matched. Fails with a timeout error if nothing matches within `timeout`, or if neither argument is given.",
            object_schema(
                &[
                    session_id,
                    ("selector", &str_t, "CSS selector to wait for."),
                    (
                        "text",
                        &str_t,
                        "Text to wait for in document.body.innerText; case-sensitive substring match.",
                    ),
                    (
                        "expression",
                        &str_t,
                        "JavaScript to poll until its value is truthy, run like evaluate (e.g. `window.dataLayer.length > 3`).",
                    ),
                    (
                        "frame",
                        &str_t,
                        "With `expression`: run it in the frame whose URL contains this (as evaluate's `frame`).",
                    ),
                    (
                        "ms",
                        &pos_int_t,
                        "With no selector, text or expression: just wait this many milliseconds (at most 25000), for hosts without timers.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait before failing. Default 10000.",
                    ),
                ],
                &["sessionId"],
            ),
            read_only(),
        ),
        tool(
            "evaluate",
            "Run JavaScript in a tab and return its value as JSON in { result }. A bare expression returns its value; for several statements use `return ...`; promises are awaited. Values that aren't JSON come back reduced: undefined as null and DOM nodes as {}. It runs in the page's main world (can read page globals like window.dataLayer); on stealth sessions (see set_stealth) it runs in an isolated world instead, which sees the DOM but not page-defined globals. `tabIndex` runs it in another tab without switching to it, and `frame` in an embedded frame. The code can change the page like any script, so prefer take_snapshot or wait_for when they suffice, and click or type for interaction. Fails with the JS exception's message if the code throws.",
            object_schema(
                &[
                    session_id,
                    (
                        "expression",
                        &str_t,
                        "JavaScript to run. Promises are awaited; the value must be JSON-serializable.",
                    ),
                    (
                        "tabIndex",
                        &nonneg_int_t,
                        "Tab to run in, by its 0-based list_pages index. Default: the active tab.",
                    ),
                    (
                        "frame",
                        &str_t,
                        "Run in the frame whose URL contains this (an embedded form from another site, e.g. `greenhouse.io`), in an isolated world that sees its DOM but not its page globals. Default: the top document.",
                    ),
                ],
                &["sessionId", "expression"],
            ),
            writes(true, true),
        ),
        tool(
            "start_profiling",
            "Start the V8 CPU profiler on a tab (active tab by default, or `tabIndex`). Use to find which JavaScript makes a page slow or unresponsive: start, reproduce the problem (navigate/click/wait), then call stop_profiling on the same tab. Don't use it for network slowness; list_network_requests shows that. The 1000µs default suits most slowness; lower `samplingIntervalUs` (e.g. 100) only to catch functions shorter than a millisecond, at the cost of more overhead and a larger profile. Profiling continues until stop_profiling, so always stop it. Returns a confirmation. Fails if that tab is already being profiled; the running recording continues.",
            object_schema(
                &[
                    session_id,
                    (
                        "tabIndex",
                        &nonneg_int_t,
                        "Tab to profile, by its 0-based list_pages index. Default: the active tab.",
                    ),
                    (
                        "samplingIntervalUs",
                        &pos_int_t,
                        "Microseconds between stack samples; smaller is more detailed but heavier. Default 1000.",
                    ),
                ],
                &["sessionId"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(false)
                .open_world(false),
        ),
        tool(
            "stop_profiling",
            "Stop the CPU profiler started by start_profiling, save a .cpuprofile under $STATE_DIR/logs/<sessionId>/profiles/ (its `path` is in the result; open it as a flame graph in the DevTools Performance panel or speedscope.app) and return the culprits: long tasks (>=50ms) split into JS vs browser-native (parse/style/layout) vs GC time with their top scripts, top JS functions and heaviest JS stack, top functions by self and total time (with url:line:col), and self time per script. Pass the same `tabIndex` given to start_profiling. If page JS is hung, the renderer can't answer until it yields, so after `terminateAfterMs` the running script is terminated to get the profile out (`terminated` in the result says so); set it to 0 to never terminate. `top` caps each ranked list, so raise it when the culprit isn't in the first 15, and keep `timeout` a few seconds above `terminateAfterMs` so a large profile has time to transfer. The file is deleted with the session by close_browser_session. Fails if start_profiling wasn't called on that tab.",
            object_schema(
                &[
                    session_id,
                    (
                        "tabIndex",
                        &nonneg_int_t,
                        "The tab passed to start_profiling, by its 0-based list_pages index. Default: the active tab.",
                    ),
                    (
                        "top",
                        &pos_int_t,
                        "How many entries to return in each ranked list. Default 15.",
                    ),
                    (
                        "terminateAfterMs",
                        &nonneg_int_t,
                        "Milliseconds to wait for hung page JS before terminating it; 0 never terminates. Default 5000.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds the whole stop call may take before failing. Default 30000.",
                    ),
                ],
                &["sessionId"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(false)
                .open_world(false),
        ),
        tool(
            "list_visits",
            "List the session's page visits, oldest first. Each visit is one top-level navigation in one tab, numbered by `seq`; pass that number as `visit` to list_console_messages or list_network_requests to see only that page load's logs. Use it to find which page load to inspect; use list_pages for the tabs open now. It reads logs and changes nothing. Empty unless the listener role is running, and deleted when the session closes.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "list_console_messages",
            "List console messages (console.log, warnings, errors, uncaught exceptions) the session's pages emitted, oldest first, from the most recent `limit`. Use it to debug page errors; use list_network_requests for failed requests. Pass `visit` (a seq from list_visits) to see one page load; without it, messages from every visit are merged and `limit` keeps the newest. It reads logs and changes nothing. Empty unless the listener role is running, nothing is captured while stealth is on, and logs are deleted when the session closes.",
            object_schema(
                &[
                    session_id,
                    (
                        "visit",
                        &pos_int_t,
                        "Only messages from this visit, by its `seq` from list_visits. Default: all visits.",
                    ),
                    (
                        "limit",
                        &pos_int_t,
                        "Return at most this many of the newest messages. Default 500.",
                    ),
                ],
                &["sessionId"],
            ),
            read_only(),
        ),
        tool(
            "list_network_requests",
            "List the network requests the session's pages made, oldest first, from the most recent `limit`: method, URL, status (or failure reason), redirects and request body. With `details`, also request and response headers and, for fetch/XHR and documents, text or JSON response bodies up to 64 KB; credential headers and keys are masked, and nothing of a login a person does in a takeover is kept. `urlContains` narrows the list to matching URLs. Use it to check API calls, what a page sent and got back, tracking pixels and failed loads; use list_console_messages for script errors and logs. Pass `visit` (a seq from list_visits) to see one page load; without it, requests from every visit are merged and `limit` keeps the newest. A null status means the request hadn't finished when logged. It reads logs and changes nothing. Empty unless the listener role is running, and logs are deleted when the session closes.",
            object_schema(
                &[
                    session_id,
                    (
                        "visit",
                        &pos_int_t,
                        "Only requests from this visit, by its `seq` from list_visits. Default: all visits.",
                    ),
                    (
                        "limit",
                        &pos_int_t,
                        "Return at most this many of the newest requests. Default 500.",
                    ),
                    (
                        "urlContains",
                        &str_t,
                        "Only requests whose URL contains this text.",
                    ),
                    (
                        "details",
                        &bool_t,
                        "Include request and response headers and response bodies. Default false.",
                    ),
                ],
                &["sessionId"],
            ),
            read_only(),
        ),
        tool(
            "set_stealth",
            "Toggle stealth for a session. `enabled: true` disables the CDP Runtime domain (removes the Runtime.enable automation tell that anti-bot systems like Cloudflare detect) — load/navigate pages in this mode to pass CDP-based bot gates; the cost is no console capture and `evaluate` runs in an isolated world (can't read page globals like window.dataLayer). `enabled: false` re-enables Runtime: console capture + main-world `evaluate` (reads page globals) return, but the CDP tell is present. Typical flow: set_stealth(true) → navigate → set_stealth(false) → evaluate. Leave it off for sites without bot detection.",
            object_schema(
                &[
                    session_id,
                    (
                        "enabled",
                        &bool_t,
                        "true turns stealth on, false turns it off.",
                    ),
                ],
                &["sessionId", "enabled"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "get_stealth",
            "Return whether stealth is on for a session; it changes nothing. Check it before evaluate or list_console_messages, whose behavior it changes; use set_stealth to change it. A new session starts with stealth off.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "set_emulation",
            "Throttle the session's CPU and/or network to measure a page as a slow phone or connection sees it (load time, hydration, layout shift, flaky taps). Applies to every tab of the session, open now or opened later (popups included), and lasts until changed or the session closes; it is in place when this returns, so navigate right after. Omitted fields keep their current value. Typical mid-range phone: cpuThrottlingRate 4 with network \"slow4g\" and useMobileUA on open_browser_session. Pass cpuThrottlingRate 1 and network \"off\" to stop throttling. Needs the listener role; fails if no tab confirms within 5s.",
            object_schema(
                &[
                    session_id,
                    (
                        "cpuThrottlingRate",
                        &num_t,
                        "CPU slowdown factor from 1 (none) to 100. 4 is a mid-range phone, 6 a low-end one.",
                    ),
                    (
                        "network",
                        &network_t,
                        "A preset (\"slow3g\", \"slow4g\", \"fast4g\" as in Chrome DevTools, \"offline\", or \"off\" for none) or custom {latencyMs, downloadKbps, uploadKbps}.",
                    ),
                ],
                &["sessionId"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "get_emulation",
            "Return the session's throttling and init scripts; it changes nothing. A new session has none.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "add_init_script",
            "Run JavaScript at the start of every page load in every tab of the session, before any of the page's own scripts, until removed or the session closes. Use it to record timings or DOM changes from the very first moment of a load (a MutationObserver, PerformanceObserver or console hook writing to a window global you read later with evaluate), or to stub an API before the page uses it. Takes effect from each tab's next navigation, not the page already loaded. It runs in the page's main world, so the page can see what it defines. Returns an id for remove_init_script. Needs the listener role.",
            object_schema(
                &[
                    session_id,
                    (
                        "source",
                        &str_t,
                        "The JavaScript to run, as a script body (not a function).",
                    ),
                ],
                &["sessionId", "source"],
            ),
            writes(false, false),
        ),
        tool(
            "remove_init_script",
            "Stop running an init script added by add_init_script; pages already loaded keep what it did. Fails on an id the session doesn't have; get_emulation lists them.",
            object_schema(
                &[
                    session_id,
                    ("id", &str_t, "The id add_init_script returned."),
                ],
                &["sessionId", "id"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(false)
                .open_world(false),
        ),
        tool(
            "request_human_takeover",
            "Get a human takeover link (URL) for the session's active page, so a person can log in themselves (passwords/passkeys the agent must not see).\nUse it only when a login needs a person; for a site you've logged into before, try load_browser_state first. Show the returned url to the user and keep the token for await_human_takeover. Does NOT block. The person starts in control, and every call on the session except reading logs fails until they hand control back; after that the link stays open for them to watch you work, and they can take control again, which pauses your calls the same way. After showing the URL, run await_human_takeover in a background task (subagent or job), since the human may take many minutes; with no background task, end your turn and call it once the user says they're done. The URL is a bearer credential that gives live control of the tab until `ttlMinutes` passes, so share it only with the intended person. Fails if TAKEOVER_BASE_URL isn't configured on the server.",
            object_schema(
                &[
                    session_id,
                    (
                        "ttlMinutes",
                        &pos_int_t,
                        "Minutes the link stays valid, 1 to 60 (larger values are capped). The link is a bearer credential, so keep it short. Default 30.",
                    ),
                ],
                &["sessionId"],
            ),
            writes(false, true),
        ),
        tool(
            "await_human_takeover",
            "Wait up to `timeout` (default 25 s) for the person to hand control back on the takeover page, or for the link to end. Returns { completed, watching, expired, url, title }: `completed` true once they handed back, with the page's URL and title; otherwise false, and call it again in a loop, each call short enough for your host's per-call limit (the default suits Executor). `watching` true means the link is still open: the person may watch and can take control again, and then calls on the session fail until you await the same token again. `expired` true means the link ran out unfinished; issue a new request_human_takeover. The person may take several minutes, so poll from a background task if you can. A session opened with a `name` saves its login under that name as soon as they finish. Fails on a token that request_human_takeover didn't issue or whose link has ended and was already reported.",
            object_schema(
                &[
                    (
                        "token",
                        &str_t,
                        "The token returned by request_human_takeover.",
                    ),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait before returning completed: false. Default 25000, max 1800000.",
                    ),
                ],
                &["token"],
            ),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(false)
                .idempotent(false)
                .open_world(false),
        ),
        tool(
            "save_browser_state",
            "Save the session's cookies under a name so a future session can load them with load_browser_state and resume without logging in again. Call it after a successful login (for example once await_human_takeover completes). Saving under an existing name replaces that state, so use a distinct name per site or account. Only cookies are saved, not localStorage, so sites that keep their login in localStorage won't resume. States are stored on disk as plaintext and include session credentials. Fails on a name with characters other than letters, digits, '.', '_' and '-'.",
            object_schema(&[session_id, state_name], &["sessionId", "name"]),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "load_browser_state",
            "Load cookies saved by save_browser_state into this session, usually right after open_browser_session and before navigate, so the session starts logged in. Pages already open need a reload or navigate to use them. Loaded cookies replace any with the same name, domain and path; others are kept. Expired logins won't work again; if the site still asks to log in, use request_human_takeover and save again. Fails if no state has that name; list_browser_states shows the names.",
            object_schema(&[session_id, state_name], &["sessionId", "name"]),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(true)
                .open_world(false),
        ),
        tool(
            "list_browser_states",
            "List the saved browser states (cookie sets saved by save_browser_state) that load_browser_state and delete_browser_state accept, sorted by name; it takes no arguments and changes nothing. Use it to pick a name to load; these are saved logins on disk, not open sessions, so use list_browser_sessions for those. The list is empty if nothing has been saved.",
            empty_obj_schema(),
            read_only(),
        ),
        tool(
            "delete_browser_state",
            "Delete a saved browser state by name, permanently; open sessions that already loaded it keep their cookies. Use it to forget a login that is stale or no longer wanted; to update a state, call save_browser_state with the same name instead. Use list_browser_states to see the names. Deleting a name that doesn't exist succeeds with removed false, so it is safe to repeat.",
            object_schema(&[state_name], &["name"]),
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(true)
                .open_world(false),
        ),
    ]
}

/// Fill in where `upload_file` reads files from: the shared uploads folder
/// when this server has one, else a note that `path` is on the server's
/// machine, which may be a container that doesn't see the agent's files.
fn with_uploads_dir(mut tools: Vec<Tool>, dir: Option<&std::path::Path>) -> Vec<Tool> {
    let note = match dir {
        Some(d) => format!(
            " Save the file into {}/ (shared with the host this server runs on; files older than a day are deleted) and pass that path.",
            d.display()
        ),
        None => " `path` is on the machine the server runs on, which may be a container that doesn't see your files; send `content` then.".to_string(),
    };
    for t in &mut tools {
        if t.name == "upload_file" {
            if let Some(d) = &t.description {
                t.description = Some(d.replace("@@UPLOADS@@", &note).into());
            }
        }
    }
    tools
}

fn tool(
    name: &'static str,
    description: &'static str,
    schema: JsonObject,
    annotations: ToolAnnotations,
) -> Tool {
    declare_output_for(
        Tool::new(name.to_string(), description.to_string(), Arc::new(schema))
            .with_annotations(annotations),
    )
}

/// Annotations for a tool that only reads.
fn read_only() -> ToolAnnotations {
    ToolAnnotations::new().read_only(true).open_world(false)
}

/// Annotations for a tool that acts on web pages: `destructive` when it can
/// trigger whatever the page does (submit, delete, buy).
fn writes(destructive: bool, open_world: bool) -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(destructive)
        .idempotent(false)
        .open_world(open_world)
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

fn object_schema(props: &[(&str, &Value, &str)], required: &[&str]) -> JsonObject {
    let mut properties = Map::new();
    for (k, v, description) in props {
        let mut prop = (*v).clone();
        prop["description"] = Value::String((*description).into());
        properties.insert((*k).into(), prop);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn upload_file_names_the_shared_folder() {
        let upload = |dir: Option<&std::path::Path>| {
            with_uploads_dir(tool_defs(), dir)
                .into_iter()
                .find(|t| t.name == "upload_file")
                .and_then(|t| t.description)
                .unwrap()
                .to_string()
        };
        let shared = upload(Some(std::path::Path::new("/var/lib/bs/uploads")));
        assert!(
            shared.contains("Save the file into /var/lib/bs/uploads/"),
            "{shared}"
        );
        let alone = upload(None);
        assert!(alone.contains("send `content` then"), "{alone}");
        assert!(!shared.contains("@@") && !alone.contains("@@"));
    }

    #[test]
    fn prose_selectors_skip_the_css_wait() {
        for prose in [
            "the login button",
            "Submit",
            "Log in",
            "Shakespeare's Sonnets",
            "the email field.",
        ] {
            assert!(looks_like_prose(prose), "{prose}");
        }
        for css in [
            "form button",
            "#nav a",
            "div.item > a",
            "input[type=email]",
            "my-widget",
            "a:hover",
            "button",
        ] {
            assert!(!looks_like_prose(css), "{css}");
        }
    }

    #[test]
    fn transport_errors_are_judged_by_type_not_text() {
        use chromiumoxide::error::CdpError;
        let dropped = anyhow::Error::from(CdpError::NoResponse).context("navigating");
        assert!(is_transport_error(&dropped));
        assert!(!is_protocol_error(&dropped));
        // A page error that merely mentions a websocket is not a dead socket.
        let page_err = anyhow!("JS exception: WebSocket is closed before the connection");
        assert!(!is_transport_error(&page_err));
        let rejected = anyhow::Error::from(CdpError::msg("Cannot find context"));
        assert!(is_protocol_error(&rejected));
        assert!(!is_transport_error(&rejected));
    }

    #[test]
    fn tool_table_matches_definitions() {
        let defined: BTreeSet<String> = tool_defs().iter().map(|t| t.name.to_string()).collect();
        let routed: BTreeSet<String> = TOOL_NAMES.iter().map(|n| n.to_string()).collect();
        assert_eq!(defined, routed);
    }

    #[test]
    fn output_schemas_are_objects() {
        for tool in tool_defs() {
            if let Some(schema) = &tool.output_schema {
                assert_eq!(schema.get("type"), Some(&json!("object")), "{}", tool.name);
            }
        }
    }
}
