//! browser-session-mcp — MCP server over stdio.
//!
//! Connects (lazily) to a persistent Chrome via the DevTools Protocol and
//! hands out isolated BrowserContexts per caller-managed sessionId.
//!
//! Required env: BROWSER_URL (e.g. http://localhost:9222), STATE_DIR.

use anyhow::{Context, Result};
use rmcp::ServiceExt;

use crate::chrome_ctx::ChromeContext;
use crate::logs::logs_dir;
use crate::saved_states::{SavedStateStore, states_dir};
use crate::server::BrowserSessionServer;
use crate::state::{StateStore, sessions_dir};

pub async fn run() -> Result<()> {
    crate::init_tracing("browser_session_mcp=info,rmcp=warn");

    let browser_url = match std::env::var("BROWSER_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("BROWSER_URL is required.");
            std::process::exit(1);
        }
    };
    let state = StateStore::load(sessions_dir())
        .await
        .context("loading state store")?;
    let ctx = ChromeContext::new(browser_url, state.clone());
    let saved_states = SavedStateStore::new(states_dir());
    let logs_dir = logs_dir();
    let srv = BrowserSessionServer::new(ctx, logs_dir, saved_states);

    tracing::info!("browser-session-mcp starting");

    let svc = srv
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .context("starting stdio service")?;
    let _ = svc.waiting().await;
    if let Err(err) = state.flush_now().await {
        tracing::warn!(error = %err, "final state flush failed");
    }
    Ok(())
}
