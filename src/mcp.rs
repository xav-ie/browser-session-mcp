//! browser-session-mcp — MCP server over stdio, or streamable HTTP.
//!
//! Connects (lazily) to a persistent Chrome via the DevTools Protocol and
//! hands out isolated BrowserContexts per caller-managed sessionId.
//!
//! Required env: BROWSER_URL (e.g. http://localhost:9222), STATE_DIR.
//! Optional: MCP_HTTP_BIND (e.g. 0.0.0.0:9224) serves MCP at `/mcp` on that
//! address instead of stdio; MCP_HTTP_ALLOWED_HOSTS (comma separated) is the
//! accepted `Host` list, loopback only by default.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;

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

    match std::env::var("MCP_HTTP_BIND") {
        Ok(bind) if !bind.is_empty() => serve_http(srv, &bind).await?,
        _ => {
            let svc = srv
                .serve((tokio::io::stdin(), tokio::io::stdout()))
                .await
                .context("starting stdio service")?;
            let _ = svc.waiting().await;
        }
    }
    if let Err(err) = state.flush_now().await {
        tracing::warn!(error = %err, "final state flush failed");
    }
    Ok(())
}

/// Serve MCP over streamable HTTP at `/mcp` until SIGINT or SIGTERM.
///
/// Stateless JSON mode, like `mcp-proxy --stateless`: no per-client session,
/// so a restart is invisible to callers. Every request is handled by a clone
/// of the one server, so all callers share the state store and the Chrome
/// connection exactly as the single stdio process does.
async fn serve_http(srv: BrowserSessionServer, bind: &str) -> Result<()> {
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    if let Ok(hosts) = std::env::var("MCP_HTTP_ALLOWED_HOSTS") {
        let hosts: Vec<String> = hosts
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(String::from)
            .collect();
        if !hosts.is_empty() {
            config.allowed_hosts = hosts;
        }
    }
    let cancel = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(srv.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding MCP_HTTP_BIND {bind}"))?;
    tracing::info!(%bind, "serving MCP over HTTP at /mcp");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(err) => {
                    tracing::warn!(error = %err, "accept failed");
                    continue;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        };
        let service = service.clone();
        tokio::spawn(async move {
            let handler = hyper::service::service_fn(move |req: http::Request<_>| {
                let service = service.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(
                        if matches!(req.uri().path(), "/mcp" | "/mcp/") {
                            service.handle(req).await
                        } else {
                            let mut not_found =
                                http::Response::new(Full::new(Bytes::new()).boxed());
                            *not_found.status_mut() = http::StatusCode::NOT_FOUND;
                            not_found
                        },
                    )
                }
            });
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), handler)
                .await
            {
                tracing::debug!(error = %err, "HTTP connection ended with an error");
            }
        });
    }
    tracing::info!("shutting down");
    cancel.cancel();
    Ok(())
}
