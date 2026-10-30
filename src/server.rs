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
use crate::jev;
use crate::logs::{self, LogKind, ReadOpts, SessionLogEntry};
use crate::profiler;
use crate::saved_states::{SavedStateStore, StateSummary};
use crate::sessions::{SessionInfo, Viewport};
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
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
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
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        let name = request.name.as_ref();
        match self.dispatch(name, args).await {
            Ok(result) => Ok(result.into()),
            Err(err) => {
                // Connection-died errors mean the next call should rebuild
                // the chromiumoxide handle from scratch.
                if looks_like_disconnect(&err) {
                    self.ctx.invalidate().await;
                }
                Ok(CallToolResult::error(vec![ContentBlock::text(format!("{err:#}"))]).into())
            }
        }
    }
}

impl BrowserSessionServer {
    // --- lifecycle ---

    async fn open_session(&self, args: JsonObject) -> Result<Structured<SessionInfo>> {
        let sessions = self.ctx.sessions().await?;
        let viewport = optional_viewport(&args, "viewport")?;
        let use_mobile = optional_bool(&args, "useMobileUA").unwrap_or(false);
        let info = sessions.open(viewport, use_mobile).await?;
        Ok(ok_text_struct(
            format!("Opened session {}", info.session_id),
            info,
        ))
    }

    async fn close_session(&self, args: JsonObject) -> Result<Text> {
        let session_id = required_str(&args, "sessionId")?.to_string();
        // Best-effort — closing an unknown sessionId is not an error.
        if let Ok(sm) = self.ctx.sessions().await {
            let _ = sm.close(&session_id).await;
        }
        let writer = logs::LogWriter::new(self.logs_dir.clone());
        let _ = writer.close_session(&session_id).await;
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
            NavigateOut { url: resolved_url },
        ))
    }

    // --- capture ---

    async fn take_screenshot(&self, args: JsonObject) -> Result<Screenshot> {
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
        Ok(Screenshot {
            text: format!("Captured {len} bytes PNG."),
            png_base64: b64,
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

    async fn click(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        // CDP Input (trusted events): reliably fires handlers AND grants user
        // activation, so it can open popups/new tabs that a synthetic JS
        // `.click()` cannot. Target a selector's center or explicit x/y.
        let (x, y, label) = if let Some(sel) = optional_str(&args, "selector") {
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, note) = resolve_and_wait(&page, sel, timeout).await?;
            let (x, y) = element_center(&page, &css).await?;
            (x, y, note.unwrap_or(css))
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

    async fn press_key(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let key = required_str(&args, "key")?.to_string();
        let page = sessions.active_page(&session_id).await?;
        // None unless Jev picked the element; names what was chosen.
        let mut note = None;
        if let Some(sel) = optional_str(&args, "selector") {
            // Focus the element, then press (e.g. type into a field then Enter).
            let timeout = optional_u64(&args, "timeout").unwrap_or(5_000);
            let (css, resolved) = resolve_and_wait(&page, sel, timeout).await?;
            note = resolved;
            page.find_element(css)
                .await
                .with_context(|| format!("locating {sel}"))?
                .press_key(key.as_str())
                .await
                .with_context(|| format!("pressing {key}"))?;
        } else {
            dispatch_key(&page, &key).await?;
        }
        let on = note.map(|n| format!(" on {n}")).unwrap_or_default();
        Ok(ok_text(format!("Pressed {key}{on}")))
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
        if delay == 0 {
            element
                .type_str(text.as_str())
                .await
                .with_context(|| format!("typing into {selector}"))?;
        } else {
            let mut buf = [0u8; 4];
            for ch in text.chars() {
                element
                    .type_str(ch.encode_utf8(&mut buf))
                    .await
                    .with_context(|| format!("typing into {selector}"))?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        let target = note.unwrap_or(selector);
        Ok(ok_text(format!(
            "Typed {} chars into {target}",
            text.chars().count()
        )))
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
        let floor = jev::min_confidence();
        let mut steps: Vec<ActStep> = Vec::new();
        let mut recent: Vec<String> = Vec::new();
        let mut url = String::new();
        let mut title = String::new();
        for step in 1..=max_steps {
            let d = match jev::decide_step(&page, &goal, &recent, &keys).await {
                Ok(d) => d,
                Err(e) => bail!("act failed at step {step}: {e:#}; {}", trail(&steps)),
            };
            url = d.page.url.clone();
            title = d.page.title.clone();
            // Gates run before executing anything this step: if the goal
            // already reads met — or nothing can help — don't act at all.
            if d.goal_met >= ACT_GATE {
                return Ok(act_done("completed", &goal, steps, url, title));
            }
            if d.stuck >= ACT_GATE {
                return Ok(act_done("stuck", &goal, steps, url, title));
            }
            if d.confidence < floor {
                bail!(
                    "act stopped at step {step}: confidence for `{}` was only {:.2} \
                     (below the {:.2} floor); {}",
                    d.description,
                    d.confidence,
                    floor,
                    trail(&steps)
                );
            }
            let el = if d.is_element {
                d.page.elements.iter().find(|e| e.id == d.chosen)
            } else {
                None
            };
            let (record, recent_line) = if let Some(el) = el {
                let css = format!("[data-jev-id=\"{}\"]", el.id);
                let (action, typed) = if !el.options.is_empty() {
                    match jev::decide_select_option(&goal, &el.desc, &el.options).await {
                        Ok(Some((opt, _))) => {
                            set_select_option(&page, &css, &opt).await?;
                            ("select".to_string(), None)
                        }
                        Ok(None) => {
                            click_label(&page, &css, &el.desc).await?;
                            ("click".to_string(), None)
                        }
                        Err(e) => bail!(
                            "act failed choosing an option at step {step}: {e:#}; {}",
                            trail(&steps)
                        ),
                    }
                } else if jev::is_text_field(&el.desc) && !values.is_empty() {
                    match jev::decide_value_key(&goal, &el.desc, &keys).await {
                        Ok(Some((key, _))) => {
                            let text = values
                                .iter()
                                .find(|(k, _)| *k == key)
                                .map(|(_, v)| v.clone())
                                .unwrap_or_default();
                            type_into(&page, &css, &text).await?;
                            ("type".to_string(), Some(key))
                        }
                        Ok(None) => {
                            click_label(&page, &css, &el.desc).await?;
                            ("click".to_string(), None)
                        }
                        Err(e) => bail!(
                            "act failed choosing a value at step {step}: {e:#}; {}",
                            trail(&steps)
                        ),
                    }
                } else {
                    click_label(&page, &css, &el.desc).await?;
                    ("click".to_string(), None)
                };
                (
                    ActStep {
                        step: step as usize,
                        action: action.clone(),
                        target: Some(el.desc.clone()),
                        typed,
                        confidence: d.confidence,
                        goal: d.goal_met,
                        stuck: d.stuck,
                    },
                    format!("{step}. {action} {}", el.desc),
                )
            } else {
                match d.chosen.as_str() {
                    "scroll_down" => scroll_viewport(&page, 0.8).await?,
                    "scroll_up" => scroll_viewport(&page, -0.8).await?,
                    "wait" => tokio::time::sleep(Duration::from_millis(500)).await,
                    other => bail!("Jev chose `{other}`, which is not one of the offered actions"),
                }
                (
                    ActStep {
                        step: step as usize,
                        action: d.chosen.clone(),
                        target: None,
                        typed: None,
                        confidence: d.confidence,
                        goal: d.goal_met,
                        stuck: d.stuck,
                    },
                    format!("{step}. {}", d.chosen),
                )
            };
            steps.push(record);
            recent.push(recent_line);
        }
        Ok(act_done("max_steps", &goal, steps, url, title))
    }

    async fn wait_for(&self, args: JsonObject) -> Result<Text> {
        let sessions = self.ctx.sessions().await?;
        let session_id = required_str(&args, "sessionId")?.to_string();
        let selector = optional_str(&args, "selector").map(str::to_string);
        let text = optional_str(&args, "text").map(str::to_string);
        let timeout = optional_u64(&args, "timeout").unwrap_or(10_000);
        if selector.is_none() && text.is_none() {
            bail!("wait_for requires either `selector` or `text`.");
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
        let wrapped = wrap_expression(&page, &expression, stealth).await?;
        let value = if stealth {
            iso_eval_function(&page, &wrapped).await
        } else {
            main_world_eval(&page, &wrapped).await
        }
        .context("evaluate")?;
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        Ok(ok_text_struct(text, EvaluateOut { result: value }))
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
                "Human takeover ready. Show this URL to the user so they can log in themselves. \
                 Then run await_human_takeover(token=\"{token}\") in a BACKGROUND task that polls \
                 with a short timeout until completed — do NOT block your main loop; the human may \
                 take several minutes:\n{url}"
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
        // How long to block. Default 5 min; capped at 30 to match the max link TTL.
        let timeout_ms = optional_u64(&args, "timeout")
            .unwrap_or(300_000)
            .min(1_800_000);
        let completed = takeover::wait_for_done(&token, Duration::from_millis(timeout_ms)).await?;
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
            TakeoverDoneOut { completed, token },
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
                        let redirect = match &n.redirected_to {
                            Some(to) => format!("  -> {to}"),
                            None => String::new(),
                        };
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
                SessionLogEntry::Network(n) => Some(n),
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

    // --- saved browser states ---

    async fn save_state(&self, args: JsonObject) -> Result<Structured<SavedOut>> {
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
            SavedOut {
                cookie_count: state.cookies.len(),
                name: state.name,
                saved_at: state.saved_at,
            },
        ))
    }

    async fn load_state(&self, args: JsonObject) -> Result<Structured<LoadedOut>> {
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
    png_base64: String,
}

impl ToolOutput for Screenshot {
    fn declare(tool: Tool) -> Tool {
        tool
    }

    fn into_result(self) -> CallToolResult {
        CallToolResult::success(vec![
            ContentBlock::text(self.text),
            ContentBlock::image(self.png_base64, "image/png".to_string()),
        ])
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
    "click" => click,
    "type" => type_text,
    "press_key" => press_key,
    "scroll" => scroll,
    "move_mouse" => move_mouse,
    "wait_for" => wait_for,
    "evaluate" => evaluate,
    "start_profiling" => start_profiling,
    "stop_profiling" => stop_profiling,
    "list_visits" => list_visits,
    "list_console_messages" => list_console_messages,
    "list_network_requests" => list_network_requests,
    "set_stealth" => set_stealth,
    "get_stealth" => get_stealth,
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
    /// "click", "type", "select", "scroll_down", "scroll_up" or "wait".
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
    /// "completed", "stuck" or "max_steps".
    status: String,
    /// The goal as given.
    goal: String,
    /// What was done, in order.
    steps: Vec<ActStep>,
    /// The page's URL at the final decision.
    url: String,
    /// The page's title at the final decision.
    title: String,
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
    /// True once the person clicked Done; false on timeout.
    completed: bool,
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
pub(crate) async fn iso_eval_function(page: &Page, function_declaration: &str) -> Result<Value> {
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
        bail!("JS exception: {}", exc.text);
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
    if let Some(goal) = selector.strip_prefix("jev:") {
        let goal = goal.trim();
        if goal.is_empty() {
            bail!("`jev:` needs a goal after the colon, e.g. `jev: the login button`");
        }
        let res = jev::resolve(page, goal).await?;
        // Extraction only offers currently-present elements, so this is an
        // instant match; the wait just covers a re-render in between.
        wait_for_selector(page, &res.css, timeout_ms).await?;
        let note = format!(
            "{} (jev: \"{goal}\", confidence {:.2}, {} offered)",
            res.description, res.confidence, res.offered
        );
        return Ok((res.css, Some(note)));
    }
    if let Err(css_err) = wait_for_selector(page, selector, timeout_ms).await {
        if !jev::available() {
            return Err(css_err);
        }
        let res = match jev::resolve_fallback(page, selector).await {
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

/// `act` stop gates: Jev's probability that the goal is already met — or
/// that no action can make progress — must cross this before the loop stops
/// on it. 0.85 matches what the jev-browser reference uses for both gates.
const ACT_GATE: f64 = 0.85;

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
) -> Structured<ActOut> {
    let n = steps.len();
    let verdict = match status {
        "completed" if n == 0 => "Goal was already met".to_string(),
        "completed" => format!("Completed the goal in {n} step{}", plural(n)),
        "stuck" => format!("Stuck after {n} step{}", plural(n)),
        _ => format!("Stopped at maxSteps with {n} step{} taken", plural(n)),
    };
    ok_text_struct(
        format!("{verdict}. Jev can be eager to call a goal met — verify the page."),
        ActOut {
            status: status.to_string(),
            goal: goal.to_string(),
            steps,
            url,
            title,
        },
    )
}

/// Scroll by a fraction of the viewport (positive = down). Used by `act`'s
/// scroll pseudo-actions.
async fn scroll_viewport(page: &Page, fraction: f64) -> Result<()> {
    let decl = format!(
        "function() {{ window.scrollBy(0, Math.round(window.innerHeight * ({fraction}))); return true; }}"
    );
    iso_eval_function(page, &decl).await?;
    Ok(())
}

/// Set a `<select>` to the option with `label`, firing input/change so page
/// scripts observe it. The element is targeted by its just-stamped
/// `data-jev-id`, i.e. still an id Jev was offered.
async fn set_select_option(page: &Page, css: &str, label: &str) -> Result<()> {
    let decl = format!(
        "function() {{ const el = document.querySelector({css}); if (!el) return false; \
         for (const o of el.options) {{ if (o.label === {label}) {{ el.value = o.value; \
         el.dispatchEvent(new Event('input', {{bubbles: true}})); \
         el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }} }} return false; }}",
        css = serde_json::to_string(css).unwrap_or_else(|_| "''".to_string()),
        label = serde_json::to_string(label).unwrap_or_else(|_| "''".to_string())
    );
    let v = iso_eval_function(page, &decl).await?;
    if v.as_bool() != Some(true) {
        bail!("option {label:?} is not in the dropdown");
    }
    Ok(())
}

/// Click an element's center through trusted CDP input, naming `label` in
/// errors — the shared path of `act`'s element clicks.
async fn click_label(page: &Page, css: &str, label: &str) -> Result<()> {
    let (x, y) = element_center(page, css).await?;
    page.click(Point::new(x, y))
        .await
        .with_context(|| format!("clicking {label}"))?;
    Ok(())
}

/// Clear a field, click it to focus, and type `text` via CDP keyboard — the
/// same sequence as the `type` tool. Used by `act` for value typing.
async fn type_into(page: &Page, css: &str, text: &str) -> Result<()> {
    let element = page
        .find_element(css.to_string())
        .await
        .with_context(|| format!("locating {css}"))?;
    let _ = element
        .call_js_fn(
            "function() { if (this.value !== undefined) this.value = ''; }",
            false,
        )
        .await;
    element.click().await.ok();
    element
        .type_str(text)
        .await
        .with_context(|| format!("typing into {css}"))?;
    Ok(())
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
    let values_t = obj(&[
        ("type", Value::String("object".into())),
        ("additionalProperties", str_t.clone()),
    ]);
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
            "Open a new isolated browser session and return its sessionId; call this first, then pass the id to every other tool. Each session is its own Chrome BrowserContext with separate cookies, storage and tabs, so sessions never see each other, and it starts with one blank tab. Don't open a new session per page: reuse one and use new_page for more tabs. To start logged in, follow with load_browser_state before navigate. Set `viewport` for a specific window size and `useMobileUA` to look like a phone; both are fixed for the session's life. Sessions persist until close_browser_session, or until the reaper role closes ones idle for MAX_IDLE_HOURS (default 24). Fails if Chrome is unreachable at BROWSER_URL.",
            object_schema(
                &[
                    (
                        "viewport",
                        &viewport_t,
                        "Window size in CSS pixels, { width, height }. Default 1280x800.",
                    ),
                    (
                        "useMobileUA",
                        &bool_t,
                        "Send a mobile User-Agent and client hints instead of desktop ones. Default false.",
                    ),
                ],
                &[],
            ),
            writes(false, false),
        ),
        tool(
            "close_browser_session",
            "Close a browser session for good: closes all its tabs, discards its cookies and storage, and deletes its console and network logs and saved .cpuprofile files. Call it when the task is done so the session doesn't sit open until the reaper closes it. Use close_tab instead to close one tab and keep the session, and save_browser_state first if the login should outlive the session. Idempotent: closing an unknown or already-closed session succeeds, so it is safe in cleanup code. Returns a confirmation.",
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
            "Load a URL in the session's active tab and return once the page's load event fires. Use new_page instead to keep the current page and load the URL in a new tab, and wait_for after this when content arrives after the load event. If every tab was closed, a new one is opened. `url` must be absolute (https://, http://, data:, about:); `timeout` bounds only the wait for the load event. The returned url is the final one after any redirects. Fails with Chrome's net error (e.g. net::ERR_NAME_NOT_RESOLVED) if the page can't load, or if `timeout` passes first, in which case the page may still be loading.",
            object_schema(
                &[
                    session_id,
                    ("url", &url_t, "Absolute URL to load, including the scheme."),
                    (
                        "timeout",
                        &pos_int_t,
                        "Milliseconds to wait for the load event before failing. Default: no limit.",
                    ),
                ],
                &["sessionId", "url"],
            ),
            writes(false, true),
        ),
        tool(
            "take_screenshot",
            "Capture a PNG of the session's active tab and return it as an image. Use it when visual layout, styling or images matter; use take_snapshot for a cheaper text view of the page's structure, and evaluate to read specific values. `fullPage` captures the whole scrollable page instead of the visible viewport, which can make a very large image on long pages. It reads the page without changing it. Returns the image plus a caption with its size in bytes.",
            object_schema(
                &[
                    session_id,
                    (
                        "fullPage",
                        &bool_t,
                        "Capture the whole scrollable page instead of only the visible viewport. Default false.",
                    ),
                ],
                &["sessionId"],
            ),
            read_only(),
        ),
        tool(
            "take_snapshot",
            "Return the accessibility tree of the session's active tab as indented text: each node's role and name (links, buttons, headings, form fields and their labels, and current values), nested by structure. Use it to find what's on the page and what to click or type into, far more cheaply than take_screenshot; use take_screenshot when visual layout matters, and evaluate for data not shown as text. It reads the page without changing it and returns the whole tree, so a large page gives long output.",
            object_schema(&[session_id], &["sessionId"]),
            read_only(),
        ),
        tool(
            "act",
            "Act toward a goal on the session's active tab in one call: the server extracts the page's interactive elements, asks the Jev decision model for the next step, does it (click, type, select, scroll or wait), and repeats — until the goal reads achieved, nothing looks able to make progress, or the step budget runs out. Each step is one cheap Jev request (~150–500ms), not a driving-LLM round trip, so multi-step flows take seconds and fractions of a cent. Pass `values` (e.g. { query: \"Oolong\" }) for anything to type: Jev only sees the keys and picks which field each belongs to — the text goes straight to the page, so secrets never reach a model. Needs TYPESAFE_API_KEY; fails closed on any Jev problem, and every step lands on an element the extraction just offered. Returns a per-step trace with confidences; `status` is completed, stuck or max_steps. Jev can be eager to call a goal met — verify with take_snapshot or take_screenshot afterwards. For a single action, click/type with a plain-language selector is cheaper.",
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
                ],
                &["sessionId", "goal"],
            ),
            writes(true, true),
        ),
        tool(
            "click",
            "Click in the session's active tab with real CDP mouse input (trusted events — fires handlers reliably and grants user activation, so it can open popups/new tabs that a JS click can't). Pass a CSS `selector` to click the first match's center, after waiting up to `timeout` for it and scrolling it into view. Plain language works too: a `jev:` prefix (e.g. `jev: the login button`) goes straight to the Jev decision model, and with TYPESAFE_API_KEY configured on the server any selector that matches nothing as CSS is retried as a plain-language goal the same way — the server sends the page's interactive elements to Jev, which picks one (on low confidence or any Jev problem it errors instead of guessing, so nothing is clicked blindly; elements are picked from the page as it is at call time, so wait_for content first if the page is still loading). Or pass both `x` and `y` to click viewport coordinates, which are used only when there is no selector. Don't use evaluate to click: synthetic JS clicks can be ignored by pages. Coordinates outside the viewport are not rejected; the click lands on nothing. A click can submit forms or trigger purchases, like a real user's. Returns a confirmation naming what was clicked. Fails if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it, or if neither a selector nor both coordinates are given.",
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
            "type",
            "Type text into the first element matching a CSS selector in the session's active tab: waits up to `timeout` for it, clicks it to focus, then sends real key events for each character, so the page's input handlers fire. Plain language works too: a `jev:` prefix (e.g. `jev: the email field`) goes straight to the Jev decision model, and with TYPESAFE_API_KEY set on the server a selector that matches nothing as CSS is retried as a plain-language goal — Jev picks the input from the page's interactive elements (it errors rather than guesses); the text itself always comes from you. Use press_key afterwards for Enter or Tab, and press_key alone for single keys. `clear` empties the field first; without it, text is appended. `delay` paces the characters for pages that react to typing speed. It doesn't check that the element is editable: typing into a button or plain text reports success but changes nothing, so target an input, textarea or contenteditable element. Returns how many characters were typed. Fails if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it.",
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
            "press_key",
            "Press a key via CDP keyboard input in the session's active tab. `key` is a name (Enter, Tab, Escape, Backspace, ArrowDown, …) or a single character. With a `selector` (CSS; or plain language — a `jev:` goal such as `jev: the password field` goes straight to the Jev decision model, and with TYPESAFE_API_KEY set a selector that matches nothing as CSS is retried as plain language), waits up to `timeout` for it and focuses it first (e.g. type into a field then press Enter); without, presses on whatever is focused, and `timeout` is unused. Use type for whole strings and click to activate buttons. Pressing Enter can submit a form, like a real user. Returns a confirmation. Fails on an unknown key name, or if the selector matches nothing within `timeout` and Jev can't (or isn't configured to) resolve it.",
            object_schema(
                &[
                    session_id,
                    (
                        "key",
                        &str_t,
                        "Key name in the US layout (Enter, Tab, Escape, Backspace, ArrowDown…) or one character.",
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
            "Wait until a CSS selector matches an element, or a text string appears in the page's visible text, in the session's active tab, polling every 100ms. Use it after navigate, click or scroll when content loads asynchronously, instead of sleeping. Pass `selector` or `text`; if both are given, only `selector` is checked. `text` is a case-sensitive substring of document.body.innerText, so text hidden with CSS doesn't count. It reads the page without changing it. Returns which selector or text matched. Fails with a timeout error if nothing matches within `timeout`, or if neither argument is given.",
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
            "Run JavaScript in a tab and return its value as JSON in { result }. A bare expression returns its value; for several statements use `return ...`; promises are awaited. Values that aren't JSON come back reduced: undefined as null and DOM nodes as {}. It runs in the page's main world (can read page globals like window.dataLayer); on stealth sessions (see set_stealth) it runs in an isolated world instead, which sees the DOM but not page-defined globals. `tabIndex` runs it in another tab without switching to it. The code can change the page like any script, so prefer take_snapshot or wait_for when they suffice, and click or type for interaction. Fails with the JS exception's message if the code throws.",
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
            "List the network requests the session's pages made, oldest first, from the most recent `limit`: method, URL, status (or failure reason), redirects and request body. Use it to check API calls, tracking pixels and failed loads; use list_console_messages for script errors and logs. Pass `visit` (a seq from list_visits) to see one page load; without it, requests from every visit are merged and `limit` keeps the newest. A null status means the request hadn't finished when logged. It reads logs and changes nothing. Empty unless the listener role is running, and logs are deleted when the session closes.",
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
            "request_human_takeover",
            "Hand the session's active page to a human to log in themselves (passwords/passkeys the agent must not see). Use it only when a login needs a person; for a site you've logged into before, try load_browser_state first. Show the returned url to the user and keep the token for await_human_takeover. Does NOT block. STRONGLY ADVISED: after showing the URL, run await_human_takeover in a BACKGROUND task (subagent/job) — not your main loop — because the human may take many minutes and you should stay responsive. The URL is a bearer credential that gives live control of the tab until `ttlMinutes` passes, so share it only with the intended person. Fails if TAKEOVER_BASE_URL isn't configured on the server.",
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
            "Wait for the human to click Done in the takeover page. `completed` is true if they finished, false on timeout. STRONGLY ADVISED: run this in a BACKGROUND task and POLL — call it in a loop with a SHORT `timeout` (e.g. 4000ms) until completed:true — rather than one long blocking call, which can exceed your MCP client's request timeout and make you miss the Done signal. The human may take several minutes; a background poller lets you be notified and resume. On completion the session is authenticated and you can continue; consider save_browser_state so later sessions skip the login. Fails on a token that request_human_takeover didn't issue or that has already completed.",
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
                        "Milliseconds to wait before returning completed: false. Default 300000, max 1800000.",
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
