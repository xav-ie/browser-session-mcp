//! Lazy + reconnect-on-failure wrapper around the Chrome connection. The MCP
//! boots cleanly even if Chrome is down — we defer the connect to the first
//! tool call so transient outages don't cascade through mcp-proxy.
use anyhow::{Context, Result};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
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

/// How long a successful probe vouches for the connection. Calls inside this
/// window skip the round trip, so a burst of tool calls costs one ping.
/// [`ChromeContext::suspect`] cuts the window short after a transport error.
const PROBE_FRESH: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct ChromeContext {
    browser_url: String,
    state: StateStore,
    /// Held only to read or swap the slot, and across a connect (so
    /// concurrent callers share one), never across a probe.
    inner: Arc<Mutex<Option<Arc<Connected>>>>,
}

struct Connected {
    sessions: Arc<SessionManager>,
    handler: JoinHandle<()>,
    /// When a probe (or the connect itself) last succeeded; `None` forces
    /// the next call to probe.
    last_ok: std::sync::Mutex<Option<Instant>>,
}

impl Connected {
    fn fresh(&self) -> bool {
        self.last_ok
            .lock()
            .expect("last_ok lock")
            .is_some_and(|t| t.elapsed() < PROBE_FRESH)
    }

    fn mark(&self, ok: Option<Instant>) {
        *self.last_ok.lock().expect("last_ok lock") = ok;
    }
}

impl Drop for Connected {
    /// Dropping a JoinHandle detaches the task rather than cancelling it, and on
    /// a half-open socket the handler would otherwise run forever. Abort it so
    /// replacing a connection actually tears the old one down.
    fn drop(&mut self) {
        self.handler.abort();
    }
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
        let cached = self.inner.lock().await.clone();
        if let Some(c) = &cached {
            // The handler task drives the chromiumoxide connection; once it
            // finishes, the WS is dead and any command sent over the cached
            // Browser would hang forever. But a half-open socket (TCP up, no
            // data flowing) leaves the handler still awaiting, so is_finished()
            // stays false while every command hangs. Actively probe with a
            // cheap CDP round trip so a wedged connection reconnects instead of
            // hanging — a bounded probe, not a per-command timeout, so slow
            // page loads are never mistaken for a dead connection. The probe
            // runs outside the lock: a slow Chrome delays this call only, not
            // every other tool call queued behind it.
            if !c.handler.is_finished() {
                if c.fresh() {
                    return Ok(c.sessions.clone());
                }
                if timeout(PROBE_TIMEOUT, c.sessions.ping())
                    .await
                    .is_ok_and(|r| r.is_ok())
                {
                    c.mark(Some(Instant::now()));
                    return Ok(c.sessions.clone());
                }
            }
        }
        let mut guard = self.inner.lock().await;
        // Replace only the connection this call found dead. If another caller
        // already swapped in a new one meanwhile, use it rather than tearing
        // down a healthy connection (and every command in flight on it).
        let replaced_elsewhere = match (guard.as_ref(), &cached) {
            (Some(now), Some(dead)) => !Arc::ptr_eq(now, dead),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if replaced_elsewhere {
            return Ok(guard.as_ref().expect("checked Some").sessions.clone());
        }
        *guard = None;
        let (browser, handler) = timeout(CONNECT_TIMEOUT, chrome::connect(&self.browser_url))
            .await
            .context("timed out connecting to Chrome")??;
        let sm = Arc::new(SessionManager::new(browser, self.state.clone()));
        *guard = Some(Arc::new(Connected {
            sessions: sm.clone(),
            handler,
            last_ok: std::sync::Mutex::new(Some(Instant::now())),
        }));
        Ok(sm)
    }

    /// Make the next `sessions()` call probe the connection instead of
    /// trusting a recent success. Call this after a tool error that came off
    /// the CDP transport: a live connection costs one ping, a dead one gets
    /// replaced, and nothing in flight is torn down on a guess.
    pub async fn suspect(&self) {
        if let Some(c) = self.inner.lock().await.as_ref() {
            c.mark(None);
        }
    }
}
