pub mod attach_guard;
pub mod chrome;
pub mod chrome_ctx;
pub mod listener;
pub mod logs;
pub mod mcp;
pub mod profiler;
pub mod reaper;
pub mod saved_states;
pub mod server;
pub mod sessions;
pub mod snapshot;
pub mod state;
pub mod takeover;
pub mod user_agent;

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Path env vars that `STATE_DIR` replaced. Refusing to start while one is set
/// turns a half-migrated config into a startup error instead of processes
/// silently reading and writing different directories.
const REPLACED_VARS: [&str; 4] = ["STATE_FILE", "LOGS_DIR", "STATES_DIR", "TAKEOVER_DIR"];

/// Resolve the required `STATE_DIR`, which every role derives its paths from
/// (`sessions/`, `logs/`, `states/`, `takeover/`). One variable means the
/// cooperating processes can't disagree about where shared files live.
pub fn init_state_dir() -> Result<()> {
    if let Some(old) = REPLACED_VARS.iter().find(|v| std::env::var_os(v).is_some()) {
        bail!(
            "{old} is no longer supported; set STATE_DIR (all paths derive from it) and unset {old}"
        );
    }
    let dir = std::env::var_os("STATE_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("STATE_DIR is required (e.g. /var/lib/browser-session-mcp)")
        })?;
    let _ = STATE_DIR.set(PathBuf::from(dir));
    Ok(())
}

/// The directory from [`init_state_dir`], which `main` calls before any role runs.
pub fn state_dir() -> &'static Path {
    STATE_DIR
        .get()
        .expect("init_state_dir must run before state_dir")
}

/// Install the tracing subscriber for a subcommand. Always logs to stderr:
/// the `mcp` subcommand speaks the MCP protocol over stdout, and stderr is
/// captured by journald for the daemons all the same.
pub fn init_tracing(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .init();
}
