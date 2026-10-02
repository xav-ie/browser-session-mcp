//! browser-session-reaper — close idle Chrome BrowserContexts and orphan tabs.
//!
//! Reads the state file written by browser-session-mcp on each tool call,
//! connects to Chrome, and disposes any context whose `lastUsedAt` is older
//! than MAX_IDLE_HOURS. With REAP_ORPHAN_TABS set, it then closes every `page`
//! target in Chrome's default context — orphans opened outside the MCP that
//! otherwise accumulate and pin a CPU core via SwiftShader. That is only safe
//! on a dedicated automation Chrome with no human browsing it, so it is off by
//! default. Prunes the state file afterwards.
//!
//! Usage: invoked from a NixOS systemd timer.
//!
//! Environment:
//!   BROWSER_URL       (default: http://127.0.0.1:9222)
//!   STATE_FILE        (default: /var/lib/browser-session-mcp/state.json)
//!   MAX_IDLE_HOURS    (default: 24)
//!   REAP_ORPHAN_TABS  (default: off; "1"/"true" closes default-context tabs)

use anyhow::{Context, Result, anyhow};
use chromiumoxide::cdp::browser_protocol::{
    browser::BrowserContextId,
    target::{CloseTargetParams, GetBrowserContextsParams, GetTargetsParams, TargetId},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::chrome;
use crate::logs::{LogWriter, default_logs_dir};
use crate::state::{default_state_file, read_state_file, write_state_file};

pub async fn run() -> Result<()> {
    crate::init_tracing("browser_session_mcp=info,rmcp=warn");
    let browser_url =
        std::env::var("BROWSER_URL").unwrap_or_else(|_| "http://127.0.0.1:9222".to_string());
    let state_file = default_state_file();
    let max_idle_hours: f64 = std::env::var("MAX_IDLE_HOURS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24.0);
    if !max_idle_hours.is_finite() || max_idle_hours <= 0.0 {
        return Err(anyhow!("invalid MAX_IDLE_HOURS: {max_idle_hours}"));
    }
    let reap_orphan_tabs = std::env::var("REAP_ORPHAN_TABS")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));

    let mut sessions = read_state_file(&state_file)
        .await
        .with_context(|| format!("reading {}", state_file.display()))?;

    let now = OffsetDateTime::now_utc();
    let cutoff_secs = (max_idle_hours * 3600.0) as i64;

    let stale: Vec<String> = sessions
        .iter()
        .filter(|(_, rec)| {
            let Ok(t) = OffsetDateTime::parse(&rec.last_used_at, &Rfc3339) else {
                return false;
            };
            (now - t).whole_seconds() > cutoff_secs
        })
        .map(|(id, _)| id.clone())
        .collect();

    println!(
        "Found {} idle session(s); connecting to Chrome…",
        stale.len()
    );

    let (browser, _handler) = chrome::connect(&browser_url).await?;
    let existing = browser
        .execute(GetBrowserContextsParams::default())
        .await
        .context("Target.getBrowserContexts")?;
    let existing_ids: std::collections::HashSet<String> = existing
        .result
        .browser_context_ids
        .iter()
        .map(|c| c.inner().to_string())
        .collect();

    let writer = LogWriter::new(default_logs_dir());
    let mut reaped = 0usize;
    let mut already_gone = 0usize;

    for session_id in &stale {
        if !existing_ids.contains(session_id) {
            already_gone += 1;
            sessions.remove(session_id);
            let _ = writer.close_session(session_id).await;
            continue;
        }
        match browser
            .dispose_browser_context(BrowserContextId::new(session_id))
            .await
        {
            Ok(_) => {
                sessions.remove(session_id);
                let _ = writer.close_session(session_id).await;
                reaped += 1;
                println!("Reaped {session_id}");
            }
            Err(err) => {
                eprintln!("Failed to close {session_id}: {err}");
            }
        }
    }

    // Sweep orphan tabs: page targets in Chrome's default context. Every MCP
    // session is a context from Target.createBrowserContext, which is exactly
    // what getBrowserContexts lists (it omits the default context), so pages in
    // a listed context are never touched here, even if the state file lost
    // track of their session. Default-context pages were opened outside the MCP
    // (stray automation, redirect chains), or by a human if this is someone's
    // own Chrome, hence the opt-in.
    let mut orphans = 0usize;
    if reap_orphan_tabs {
        let targets = browser
            .execute(GetTargetsParams::default())
            .await
            .context("Target.getTargets")?;
        for info in &targets.result.target_infos {
            if info.r#type != "page" {
                continue;
            }
            let in_session_context = info
                .browser_context_id
                .as_ref()
                .is_some_and(|id| existing_ids.contains(id.inner()));
            if in_session_context {
                continue;
            }
            let target_id = TargetId::new(info.target_id.inner());
            match browser.execute(CloseTargetParams::new(target_id)).await {
                Ok(_) => {
                    orphans += 1;
                    println!(
                        "Closed orphan tab {} ({})",
                        info.target_id.inner(),
                        info.url
                    );
                }
                Err(err) => eprintln!("Failed to close orphan {}: {err}", info.target_id.inner()),
            }
        }
    }

    write_state_file(&state_file, &sessions)
        .await
        .with_context(|| format!("writing {}", state_file.display()))?;
    println!(
        "Done. reaped={reaped} already-gone={already_gone} orphans={orphans} remaining={}",
        sessions.len()
    );
    Ok(())
}
