//! Lazy + reconnect-on-failure wrapper around the Chrome connection. The MCP
//! boots cleanly even if Chrome is down — we defer the connect to the first
//! tool call so transient outages don't cascade through mcp-proxy.
use anyhow::{Context, Result};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};

use crate::chrome;
use crate::sessions::SessionManager;
use crate::state::StateStore;

/// Cap the initial connect so a wedged Chrome/proxy surfaces as an error the
/// caller can retry, instead of hanging the tool call forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap the liveness probe on a cached connection. Short, since it's a single
/// cheap CDP round trip — anything slower than this means the socket is wedged.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct ChromeContext {
    browser_url: String,
    state: StateStore,
    inner: Arc<Mutex<Option<Connected>>>,
}

struct Connected {
    sessions: Arc<SessionManager>,
    handler: JoinHandle<()>,
}

impl ChromeContext {
    pub fn new(browser_url: String, state: StateStore) -> Self {
        Self {
            browser_url,
            state,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub fn state(&self) -> &StateStore {
        &self.state
    }

    pub async fn sessions(&self) -> Result<Arc<SessionManager>> {
        let mut guard = self.inner.lock().await;
        if let Some(c) = guard.as_ref() {
            // The handler task drives the chromiumoxide connection; once it
            // finishes, the WS is dead and any command sent over the cached
            // Browser would hang forever. But a half-open socket (TCP up, no
            // data flowing) leaves the handler still awaiting, so is_finished()
            // stays false while every command hangs. Actively probe with a
            // cheap CDP round trip so a wedged connection reconnects instead of
            // hanging — a bounded probe, not a per-command timeout, so slow
            // page loads are never mistaken for a dead connection.
            let alive = !c.handler.is_finished()
                && timeout(PROBE_TIMEOUT, c.sessions.ping())
                    .await
                    .is_ok_and(|r| r.is_ok());
            if alive {
                return Ok(c.sessions.clone());
            }
            *guard = None;
        }
        let (browser, handler) = timeout(CONNECT_TIMEOUT, chrome::connect(&self.browser_url))
            .await
            .context("timed out connecting to Chrome")??;
        let sm = Arc::new(SessionManager::new(browser, self.state.clone()));
        *guard = Some(Connected {
            sessions: sm.clone(),
            handler,
        });
        Ok(sm)
    }

    /// Force the next `sessions()` call to reconnect. Call this from tool
    /// handlers after an error that looks like a dropped connection.
    pub async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }
}
