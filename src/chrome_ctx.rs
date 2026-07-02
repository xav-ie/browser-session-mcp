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
            // Browser would hang forever waiting on a response that never
            // comes. Drop the stale entry and reconnect instead.
            if c.handler.is_finished() {
                *guard = None;
            } else {
                return Ok(c.sessions.clone());
            }
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
