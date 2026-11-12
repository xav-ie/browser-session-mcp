//! Maps the public sessionId (string) onto a chromiumoxide BrowserContextId.
//!
//! sessionId == BrowserContextId, so sessions survive this MCP subprocess
//! restarting — a fresh subprocess can connect to Chrome and find the same
//! contexts by id.
//!
//! Console + network capture lives in the listener daemon; this module only
//! handles the lifecycle + active page lookup.
use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::Browser;
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::{
    browser::BrowserContextId,
    emulation::{SetDeviceMetricsOverrideParams, SetTouchEmulationEnabledParams},
    network::SetUserAgentOverrideParams,
    page::GetLayoutMetricsParams,
    target::{
        CloseTargetParams, CreateBrowserContextParams, CreateTargetParams, GetTargetsParams,
        TargetId, TargetInfo,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::emulation::{self, Emulation, InitScript};
use crate::state::{SessionRecord, StateStore};
use crate::user_agent::{self, UaOverride};

const EMULATION_APPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionInfo {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "pageCount")]
    pub page_count: usize,
    #[serde(rename = "activeUrl")]
    pub active_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Viewport {
    pub width: i64,
    pub height: i64,
    /// Device pixels per CSS pixel (`window.devicePixelRatio`).
    #[serde(
        rename = "deviceScaleFactor",
        default = "one",
        skip_serializing_if = "is_one"
    )]
    pub scale: f64,
}

fn one() -> f64 {
    1.0
}

fn is_one(n: &f64) -> bool {
    *n == 1.0
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            scale: 1.0,
        }
    }
}

/// In-memory, per-session tab ordering + active-tab pointer. Chrome's
/// `Target.getTargets` order is volatile (a tab that re-creates its target —
/// e.g. Tag Assistant — keeps jumping to the end), so we maintain our own
/// stable order: existing tabs keep their slot, new tabs append, closed tabs
/// drop. Not persisted (rebuilt from Chrome on the first call after a restart).
#[derive(Default)]
struct TabRegistry {
    /// session_id -> ordered targetIds
    order: HashMap<String, Vec<String>>,
    /// session_id -> active targetId
    active: HashMap<String, String>,
}

pub struct SessionManager {
    browser: Browser,
    state: StateStore,
    tabs: Arc<Mutex<TabRegistry>>,
    /// Tabs this process watches for dialogs, by targetId.
    guarded: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Dialogs answered on each session's tabs since the last action read
    /// them, by session.
    dialogs: Arc<std::sync::Mutex<HashMap<String, Vec<String>>>>,
}

impl SessionManager {
    pub fn new(browser: Browser, state: StateStore) -> Self {
        Self {
            browser,
            state,
            tabs: Arc::new(Mutex::new(TabRegistry::default())),
            guarded: Arc::new(Mutex::new(HashMap::new())),
            dialogs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Reconcile our stable tab order for a session against Chrome's live page
    /// targets. Returns the ordered TargetInfos (stable slots) and the active
    /// targetId (guaranteed live, or None if the session has no tabs).
    async fn reconcile_tabs(&self, session_id: &str) -> Result<(Vec<TargetInfo>, Option<String>)> {
        let mut current: Vec<TargetInfo> = self
            .page_targets()
            .await?
            .into_iter()
            .filter(|t| {
                t.browser_context_id
                    .as_ref()
                    .map(|c| c.inner() == session_id)
                    .unwrap_or(false)
            })
            .collect();
        let live: Vec<String> = current
            .iter()
            .map(|t| t.target_id.inner().to_string())
            .collect();

        let (ordered_ids, active) = {
            let mut reg = self.tabs.lock().await;
            let order = reg.order.entry(session_id.to_string()).or_default();
            order.retain(|id| live.contains(id));
            for id in &live {
                if !order.contains(id) {
                    order.push(id.clone());
                }
            }
            let ordered_ids = order.clone();
            let active = reg
                .active
                .get(session_id)
                .cloned()
                .filter(|a| live.contains(a))
                .or_else(|| ordered_ids.last().cloned());
            if let Some(ref id) = active {
                reg.active.insert(session_id.to_string(), id.clone());
            }
            (ordered_ids, active)
        };

        let mut by_id: HashMap<String, TargetInfo> = current
            .drain(..)
            .map(|t| (t.target_id.inner().to_string(), t))
            .collect();
        let ordered: Vec<TargetInfo> = ordered_ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .collect();
        Ok((ordered, active))
    }

    /// Mark a target as this session's active tab (used by `new_page` /
    /// `switch_tab`).
    async fn set_active(&self, session_id: &str, target_id: &str) {
        let mut reg = self.tabs.lock().await;
        reg.active
            .insert(session_id.to_string(), target_id.to_string());
    }

    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    /// Cheap liveness probe: one `Target.getTargets` round trip. Errors if the
    /// underlying WS is dead; hangs if it's half-open, so callers must bound it
    /// with a timeout (see `ChromeContext::sessions`).
    pub async fn ping(&self) -> Result<()> {
        self.page_targets().await.map(|_| ())
    }

    pub async fn open(
        &self,
        viewport: Option<Viewport>,
        use_mobile_ua: bool,
        custom_ua: Option<&str>,
    ) -> Result<SessionInfo> {
        let mut override_ = user_agent::resolve(&self.browser, use_mobile_ua).await?;
        if let Some(ua) = custom_ua {
            // A preset's `{chrome}` is the running Chrome's major version,
            // so its User-Agent matches the browser's client hints.
            let major = override_
                .user_agent
                .split("Chrome/")
                .nth(1)
                .and_then(|v| v.split('.').next())
                .unwrap_or("")
                .to_string();
            let ua = ua.replace("{chrome}", &major);
            if !ua.contains("Chrome/") {
                // Another browser (Safari, Firefox) sends no client hints of
                // its own, so only a Chrome User-Agent keeps the Chrome ones.
                override_.metadata = serde_json::Value::Null;
            } else if let Some(model) = ua
                .split("Android ")
                .nth(1)
                .and_then(|r| r.split(')').next())
                .and_then(|r| r.split("; ").nth(1))
            {
                // An Android phone's client hints name its model too.
                override_.metadata["model"] = serde_json::Value::String(model.to_string());
            }
            override_.user_agent = ua;
        }
        let ctx_id = self
            .browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await
            .context("Target.createBrowserContext")?;
        let session_id = ctx_id.inner().to_string();

        self.state
            .set_user_agent_override(&session_id, &override_)
            .await;

        // Create the initial page in the new context. If anything below fails
        // we dispose the context AND drop the state record so we don't leak a
        // half-initialized session for the reaper to clean up later.
        let page = match self
            .browser
            .new_page(
                CreateTargetParams::builder()
                    .url("about:blank")
                    .browser_context_id(ctx_id.clone())
                    .build()
                    .map_err(|e| anyhow!("CreateTargetParams: {e}"))?,
            )
            .await
        {
            Ok(p) => p,
            Err(err) => {
                self.cleanup_failed_open(&session_id, ctx_id).await;
                return Err(anyhow!(err).context("Target.createTarget"));
            }
        };

        if let Err(err) = self.apply_user_agent(&page, &override_).await {
            self.cleanup_failed_open(&session_id, ctx_id).await;
            return Err(err);
        }

        let vp = viewport.unwrap_or_default();
        if let Err(err) = self.apply_viewport(&page, vp, use_mobile_ua).await {
            self.cleanup_failed_open(&session_id, ctx_id).await;
            return Err(err);
        }
        self.state.set_viewport(&session_id, vp).await;
        // Write now rather than on the flusher's delay: the listener's attach
        // guard reads this session's UA from disk to mask popups, and a page can
        // open one within that second.
        if let Err(err) = self.state.flush_now().await {
            tracing::warn!(error = %err, "state flush after open failed");
        }

        self.state.touch(&session_id).await;
        Ok(SessionInfo {
            session_id,
            page_count: 1,
            active_url: Some("about:blank".to_string()),
        })
    }

    async fn cleanup_failed_open(&self, session_id: &str, ctx_id: BrowserContextId) {
        let _ = self.browser.dispose_browser_context(ctx_id).await;
        self.state.forget(session_id).await;
    }

    /// The name a session was opened with, if any.
    pub async fn name(&self, session_id: &str) -> Option<String> {
        self.state.name(session_id).await
    }

    /// The live session opened with `name`, if one is still open. Records
    /// of named sessions Chrome no longer has (it restarted, or the reaper
    /// closed them) are dropped.
    pub async fn find_named(&self, name: &str) -> Result<Option<String>> {
        let ids = self.state.named(name).await;
        if ids.is_empty() {
            return Ok(None);
        }
        let live: std::collections::HashSet<String> = self
            .list()
            .await?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        let mut found = None;
        for id in ids {
            if live.contains(&id) {
                found = Some(id);
            } else {
                self.state.forget(&id).await;
            }
        }
        Ok(found)
    }

    pub async fn set_name(&self, session_id: &str, name: &str) {
        self.state.set_name(session_id, name).await;
    }

    pub async fn close(&self, session_id: &str) -> Result<()> {
        let ctx_id = parse_context_id(session_id);
        self.browser
            .dispose_browser_context(ctx_id)
            .await
            .context("Target.disposeBrowserContext")?;
        self.state.forget(session_id).await;
        {
            let mut reg = self.tabs.lock().await;
            reg.order.remove(session_id);
            reg.active.remove(session_id);
        }
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        let targets = self.page_targets().await?;
        let mut by_ctx: std::collections::BTreeMap<String, Vec<TargetInfo>> =
            std::collections::BTreeMap::new();
        for t in targets {
            if let Some(ref ctx) = t.browser_context_id {
                by_ctx.entry(ctx.inner().to_string()).or_default().push(t);
            }
        }
        let mut out = Vec::new();
        for (session_id, pages) in by_ctx {
            let active_url = pages.last().map(|p| p.url.clone());
            out.push(SessionInfo {
                session_id,
                page_count: pages.len(),
                active_url,
            });
        }
        Ok(out)
    }

    pub async fn context_id_for(&self, session_id: &str) -> Result<BrowserContextId> {
        // Must hit Chrome to confirm the context still exists, since the MCP
        // process can be recycled out of sync with reality.
        let targets = self.page_targets().await?;
        let exists = targets.iter().any(|t| {
            t.browser_context_id
                .as_ref()
                .map(|c| c.inner() == session_id)
                .unwrap_or(false)
        });
        if !exists {
            // Edge case: a context with no pages still exists. Check
            // Target.getBrowserContexts to be sure before erroring out.
            let contexts = self.list_context_ids().await?;
            if !contexts.iter().any(|c| c.inner() == session_id) {
                bail!("Session not found: {session_id}. Call open_browser_session first.");
            }
        }
        self.state.touch(session_id).await;
        Ok(parse_context_id(session_id))
    }

    pub async fn active_page(&self, session_id: &str) -> Result<Page> {
        let _ctx = self.context_id_for(session_id).await?;
        let (_ordered, active) = self.reconcile_tabs(session_id).await?;
        let target_id = match active {
            Some(id) => id,
            None => {
                // No page yet — open one (becomes active) and apply UA.
                return self.new_page(session_id, None).await;
            }
        };
        let page = self
            .browser
            .get_page(TargetId::new(target_id.as_str()))
            .await
            .context("Browser::get_page")?;
        self.prepare(session_id, &page).await?;
        Ok(page)
    }

    /// The session's tabs in stable order, plus the active targetId.
    pub async fn tabs(&self, session_id: &str) -> Result<(Vec<TargetInfo>, Option<String>)> {
        let _ctx = self.context_id_for(session_id).await?;
        self.reconcile_tabs(session_id).await
    }

    /// Get the page at a stable tab index (matches `list_pages` ordering).
    pub async fn page_at(&self, session_id: &str, index: usize) -> Result<Page> {
        let _ctx = self.context_id_for(session_id).await?;
        let (ordered, _active) = self.reconcile_tabs(session_id).await?;
        let n = ordered.len();
        let t = ordered
            .get(index)
            .ok_or_else(|| anyhow!("no tab at index {index} (session has {n} tabs)"))?;
        let page = self
            .browser
            .get_page(TargetId::new(t.target_id.inner()))
            .await
            .context("Browser::get_page")?;
        self.prepare(session_id, &page).await?;
        Ok(page)
    }

    /// Make the tab at `index` active (subsequent active-page ops target it) and
    /// bring it to the front in Chrome. Returns its url.
    pub async fn switch_tab(&self, session_id: &str, index: usize) -> Result<String> {
        let page = self.page_at(session_id, index).await?;
        self.set_active(session_id, &page.target_id().inner().to_string())
            .await;
        let _ = page.activate().await;
        let _ = page.bring_to_front().await;
        Ok(page.url().await?.unwrap_or_default())
    }

    /// Close the tab at `index`. Drops it from the registry; the active pointer
    /// falls back to the last remaining tab on the next reconcile.
    pub async fn close_tab(&self, session_id: &str, index: usize) -> Result<()> {
        let (ordered, _active) = self.tabs(session_id).await?;
        let n = ordered.len();
        let t = ordered
            .get(index)
            .ok_or_else(|| anyhow!("no tab at index {index} (session has {n} tabs)"))?;
        let target_id = t.target_id.inner().to_string();
        self.browser
            .execute(CloseTargetParams::new(TargetId::new(target_id.as_str())))
            .await
            .context("Target.closeTarget")?;
        let mut reg = self.tabs.lock().await;
        if let Some(order) = reg.order.get_mut(session_id) {
            order.retain(|id| id != &target_id);
        }
        if reg.active.get(session_id) == Some(&target_id) {
            reg.active.remove(session_id);
        }
        Ok(())
    }

    /// Change the session's emulation or init scripts and wait for the attach
    /// guard to apply the change to every open tab. A session with no tab has
    /// nothing to wait for: its next tab gets the change when it attaches.
    pub async fn update_emulation(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut Option<Emulation>, &mut Vec<InitScript>),
    ) -> Result<SessionRecord> {
        let _ctx = self.context_id_for(session_id).await?;
        let rec = self.state.update_emulation(session_id, f).await?;
        let (tabs, _) = self.reconcile_tabs(session_id).await?;
        if tabs.is_empty() {
            return Ok(rec);
        }
        let deadline = tokio::time::Instant::now() + EMULATION_APPLY_TIMEOUT;
        loop {
            if emulation::applied_revision(session_id)
                .await
                .is_some_and(|r| r >= rec.emulation_rev)
            {
                return Ok(rec);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "saved, but no tab confirmed it within {}s: the listener role applies emulation and init scripts, so check that it is running",
                    EMULATION_APPLY_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn record(&self, session_id: &str) -> Result<SessionRecord> {
        let _ctx = self.context_id_for(session_id).await?;
        Ok(self.state.record(session_id).await.unwrap_or_default())
    }

    /// Whether the session emulates a phone (opened with `useMobileUA`), so
    /// pointing input should be touch.
    pub async fn is_mobile(&self, session_id: &str) -> bool {
        self.state.is_mobile(session_id).await
    }

    /// The viewport chosen at `open` (or its default for older sessions).
    pub async fn viewport(&self, session_id: &str) -> Viewport {
        self.state.viewport(session_id).await.unwrap_or_default()
    }

    /// The CDP targetId of the session's active page, creating a page if the
    /// context has none yet. Used to build the `/devtools/page/<id>` WebSocket
    /// path that the human-takeover page connects to directly. Mirrors
    /// `active_page`'s "most-recently-opened tab" selection.
    pub async fn active_target_id(&self, session_id: &str) -> Result<String> {
        let _ctx = self.context_id_for(session_id).await?;
        if let (_, Some(active)) = self.reconcile_tabs(session_id).await? {
            return Ok(active);
        }
        // No page in this context yet — open one, then re-resolve.
        self.new_page(session_id, None).await?;
        self.reconcile_tabs(session_id)
            .await?
            .1
            .ok_or_else(|| anyhow!("no page target for session {session_id} after creating one"))
    }

    pub async fn new_page(&self, session_id: &str, url: Option<&str>) -> Result<Page> {
        let ctx_id = self.context_id_for(session_id).await?;
        // Open blank, apply the UA override, and only then navigate. Creating
        // the target with the real URL starts that request before the override
        // lands, so it goes out with the browser's own UA while the page's JS
        // later reports the override: a mismatch detectors look for.
        let page = self
            .browser
            .new_page(
                CreateTargetParams::builder()
                    .url("about:blank")
                    .browser_context_id(ctx_id)
                    .build()
                    .map_err(|e| anyhow!("CreateTargetParams: {e}"))?,
            )
            .await
            .context("Target.createTarget")?;
        if let Some(override_) = self.state.user_agent_override(session_id).await {
            self.apply_user_agent(&page, &override_).await?;
        }
        self.prepare(session_id, &page).await?;
        if let Some(url) = url {
            page.goto(url).await.context("Page.navigate")?;
        }
        // A freshly opened tab becomes the active one (matches prior behavior).
        self.set_active(session_id, &page.target_id().inner().to_string())
            .await;
        Ok(page)
    }

    pub async fn pages(&self, session_id: &str) -> Result<Vec<Page>> {
        let _ctx = self.context_id_for(session_id).await?;
        let (ordered, _active) = self.reconcile_tabs(session_id).await?;
        let mut out = Vec::new();
        for t in ordered {
            if let Ok(p) = self
                .browser
                .get_page(TargetId::new(t.target_id.inner()))
                .await
            {
                out.push(p);
            }
        }
        Ok(out)
    }

    async fn page_targets(&self) -> Result<Vec<TargetInfo>> {
        let mut result = self
            .browser
            .execute(GetTargetsParams::default())
            .await
            .context("Target.getTargets")?;
        let target_infos = std::mem::take(&mut result.result.target_infos);
        Ok(target_infos
            .into_iter()
            .filter(|t| t.r#type == "page")
            .collect())
    }

    async fn list_context_ids(&self) -> Result<Vec<BrowserContextId>> {
        use chromiumoxide::cdp::browser_protocol::target::GetBrowserContextsParams;
        let mut result = self
            .browser
            .execute(GetBrowserContextsParams::default())
            .await
            .context("Target.getBrowserContexts")?;
        Ok(std::mem::take(&mut result.result.browser_context_ids))
    }

    async fn apply_user_agent(&self, page: &Page, override_: &UaOverride) -> Result<()> {
        let params: SetUserAgentOverrideParams = serde_json::from_value(serde_json::json!({
            "userAgent": override_.user_agent,
            "userAgentMetadata": override_.metadata,
        }))
        .context("constructing SetUserAgentOverrideParams from UA override")?;
        page.execute(params)
            .await
            .context("Network.setUserAgentOverride")?;
        Ok(())
    }

    /// Make sure `page` renders at the session viewport, re-applying it only
    /// when it doesn't. Checked against the live layout rather than remembered:
    /// popups the page opens start at Chrome's window size, and the takeover
    /// UI's own override replaces ours and, once its connection closes, leaves
    /// the tab at the raw window size. `Page.getLayoutMetrics` runs no page JS.
    /// Sessions from before viewports were stored get the `open` default.
    /// Ready a tab for a tool call: its viewport, and once per tab per
    /// process, answers to the page's own dialogs and no passkey prompts.
    async fn prepare(&self, session_id: &str, page: &Page) -> Result<()> {
        self.ensure_viewport(session_id, page).await?;
        let target = page.target_id().inner().to_string();
        let mut guarded = self.guarded.lock().await;
        if guarded.get(&target).is_some_and(|t| !t.is_finished()) {
            return Ok(());
        }
        no_passkeys(page).await;
        guarded.insert(target, self.answer_dialogs(session_id, page).await?);
        Ok(())
    }

    /// Accept the page's JavaScript dialogs (alert, confirm, prompt, and
    /// "Leave site?" on unload) as they open, noting each for the session.
    /// Chrome holds the tab until a dialog is answered, so an unanswered one
    /// hangs every later call on it (navigate, evaluate, screenshots).
    async fn answer_dialogs(
        &self,
        session_id: &str,
        page: &Page,
    ) -> Result<tokio::task::JoinHandle<()>> {
        use chromiumoxide::cdp::browser_protocol::page::{
            EventJavascriptDialogOpening, HandleJavaScriptDialogParams,
        };
        use futures::StreamExt;
        let mut events = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .context("listening for JavaScript dialogs")?;
        let (page, dialogs, session) = (page.clone(), self.dialogs.clone(), session_id.to_string());
        Ok(tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                let kind = format!("{:?}", ev.r#type).to_lowercase();
                let said = if ev.message.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind}: {}", ev.message)
                };
                if let Ok(mut d) = dialogs.lock() {
                    d.entry(session.clone()).or_default().push(said);
                }
                // Another watcher (act's) may have answered it already.
                let _ = page.execute(HandleJavaScriptDialogParams::new(true)).await;
            }
        }))
    }

    /// Dialogs answered on the session's tabs since the last call to this.
    pub fn take_dialogs(&self, session_id: &str) -> Vec<String> {
        self.dialogs
            .lock()
            .ok()
            .and_then(|mut d| d.remove(session_id))
            .unwrap_or_default()
    }

    async fn ensure_viewport(&self, session_id: &str, page: &Page) -> Result<()> {
        let vp = self.state.viewport(session_id).await.unwrap_or_default();
        let metrics = page
            .execute(GetLayoutMetricsParams::default())
            .await
            .context("Page.getLayoutMetrics")?;
        let layout = &metrics.result.css_layout_viewport;
        // The layout viewport excludes scrollbars, so a scrolling page measures
        // up to a scrollbar's width (15px here) under the emulated size.
        let fits = |actual: i64, want: i64| actual <= want && want - actual <= 20;
        let mobile = self.state.is_mobile(session_id).await;
        if mobile {
            // Touch emulation ends with the CDP session that set it (another
            // MCP process, a restart), and setting it again is idempotent.
            apply_touch(page).await?;
        }
        // The pixel ratio doesn't show in the layout size, so a session that
        // sets one has it applied every time (it's idempotent).
        if vp.scale == 1.0
            && fits(layout.client_width, vp.width)
            && fits(layout.client_height, vp.height)
        {
            return Ok(());
        }
        self.apply_viewport(page, vp, mobile).await
    }

    /// Size the page and, for a mobile session, emulate a phone: `mobile`
    /// device metrics (the page's `<meta name=viewport>` applies, scrollbars
    /// overlay) and a touch screen.
    async fn apply_viewport(&self, page: &Page, vp: Viewport, mobile: bool) -> Result<()> {
        if mobile {
            apply_touch(page).await?;
        }
        let params = SetDeviceMetricsOverrideParams::builder()
            .width(vp.width)
            .height(vp.height)
            .device_scale_factor(vp.scale)
            // `screen.width`/`height` too: sites read them to tell a phone
            // from a desktop, and left alone they report the window.
            .screen_width(vp.width)
            .screen_height(vp.height)
            .mobile(mobile)
            .build()
            .map_err(|e| anyhow!("SetDeviceMetricsOverrideParams: {e}"))?;
        page.execute(params)
            .await
            .context("Emulation.setDeviceMetricsOverride")?;
        Ok(())
    }
}

/// Turn passkeys off on a tab: a virtual authenticator that fails every
/// request at once, the way a person cancelling the prompt does, so sites
/// fall back to a password or a code. Without one, a passkey request waits
/// on a browser prompt nobody can answer and the page stalls. Passkeys can't
/// be forwarded to a person's own devices: a passkey only signs for its own
/// site, and their browser enforces that against the takeover page. Best
/// effort.
async fn no_passkeys(page: &Page) {
    use chromiumoxide::cdp::browser_protocol::web_authn::{
        AddVirtualAuthenticatorParams, AuthenticatorProtocol, AuthenticatorTransport, EnableParams,
        VirtualAuthenticatorOptions,
    };
    let enable = EnableParams::builder().enable_ui(false).build();
    if page.execute(enable).await.is_err() {
        return;
    }
    let Ok(options) = VirtualAuthenticatorOptions::builder()
        .protocol(AuthenticatorProtocol::Ctap2)
        .transport(AuthenticatorTransport::Internal)
        .has_resident_key(true)
        .has_user_verification(true)
        .is_user_verified(false)
        .automatic_presence_simulation(true)
        .build()
    else {
        return;
    };
    let _ = page
        .execute(AddVirtualAuthenticatorParams::new(options))
        .await;
}

/// How many touch points a mobile session's screen reports, as a phone does.
pub const TOUCH_POINTS: i64 = 5;

/// Make the page see a touch screen: `navigator.maxTouchPoints`, touch
/// events and `(pointer: coarse)`, which sites use to pick touch behavior.
async fn apply_touch(page: &Page) -> Result<()> {
    page.execute(
        SetTouchEmulationEnabledParams::builder()
            .enabled(true)
            .max_touch_points(TOUCH_POINTS)
            .build()
            .map_err(|e| anyhow!("SetTouchEmulationEnabledParams: {e}"))?,
    )
    .await
    .context("Emulation.setTouchEmulationEnabled")?;
    Ok(())
}

fn parse_context_id(session_id: &str) -> BrowserContextId {
    BrowserContextId::new(session_id.to_string())
}
