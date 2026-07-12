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
    emulation::SetDeviceMetricsOverrideParams,
    network::SetUserAgentOverrideParams,
    page::AddScriptToEvaluateOnNewDocumentParams,
    target::{
        CloseTargetParams, CreateBrowserContextParams, CreateTargetParams, GetTargetsParams,
        TargetId, TargetInfo,
    },
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::state::StateStore;
use crate::user_agent::{self, UaOverride};

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "pageCount")]
    pub page_count: usize,
    #[serde(rename = "activeUrl")]
    pub active_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: i64,
    pub height: i64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
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
}

impl SessionManager {
    pub fn new(browser: Browser, state: StateStore) -> Self {
        Self {
            browser,
            state,
            tabs: Arc::new(Mutex::new(TabRegistry::default())),
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
    ) -> Result<SessionInfo> {
        let override_ = user_agent::resolve(&self.browser, use_mobile_ua).await?;
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

        if let Err(err) = self.apply_stealth(&page).await {
            self.cleanup_failed_open(&session_id, ctx_id).await;
            return Err(err);
        }

        let vp = viewport.unwrap_or_default();
        if let Err(err) = self.apply_viewport(&page, vp).await {
            self.cleanup_failed_open(&session_id, ctx_id).await;
            return Err(err);
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
        self.browser
            .get_page(TargetId::new(t.target_id.inner()))
            .await
            .context("Browser::get_page")
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
        let target_url = url.unwrap_or("about:blank").to_string();
        let page = self
            .browser
            .new_page(
                CreateTargetParams::builder()
                    .url(target_url)
                    .browser_context_id(ctx_id)
                    .build()
                    .map_err(|e| anyhow!("CreateTargetParams: {e}"))?,
            )
            .await
            .context("Target.createTarget")?;
        if let Some(override_) = self.state.user_agent_override(session_id).await {
            self.apply_user_agent(&page, &override_).await?;
        }
        self.apply_stealth(&page).await?;
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
            // Real browsers send Accept-Language + navigator.languages; an empty
            // value is a headless tell.
            "acceptLanguage": "en-US,en;q=0.9",
        }))
        .context("constructing SetUserAgentOverrideParams from UA override")?;
        page.execute(params)
            .await
            .context("Network.setUserAgentOverride")?;
        Ok(())
    }

    /// Patch the JS-visible signals bot-detectors (e.g. Cloudflare) use to spot
    /// automated/headless Chrome, before any page script runs. Pairs with the
    /// `--disable-blink-features=AutomationControlled` launch flag. Best-effort
    /// and an arms race — covers the common tells, not a guarantee.
    async fn apply_stealth(&self, page: &Page) -> Result<()> {
        let params = AddScriptToEvaluateOnNewDocumentParams::builder()
            .source(user_agent::STEALTH_JS)
            .build()
            .map_err(|e| anyhow!("AddScriptToEvaluateOnNewDocumentParams: {e}"))?;
        page.execute(params)
            .await
            .context("Page.addScriptToEvaluateOnNewDocument")?;
        Ok(())
    }

    async fn apply_viewport(&self, page: &Page, vp: Viewport) -> Result<()> {
        let params = SetDeviceMetricsOverrideParams::builder()
            .width(vp.width)
            .height(vp.height)
            .device_scale_factor(1.0)
            .mobile(false)
            .build()
            .map_err(|e| anyhow!("SetDeviceMetricsOverrideParams: {e}"))?;
        page.execute(params)
            .await
            .context("Emulation.setDeviceMetricsOverride")?;
        Ok(())
    }
}

fn parse_context_id(session_id: &str) -> BrowserContextId {
    BrowserContextId::new(session_id.to_string())
}
