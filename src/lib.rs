pub mod chrome;
pub mod chrome_ctx;
pub mod listener;
pub mod logs;
pub mod mcp;
pub mod reaper;
pub mod saved_states;
pub mod server;
pub mod sessions;
pub mod snapshot;
pub mod state;
pub mod takeover;
pub mod user_agent;

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Install the tracing subscriber for a subcommand. Always logs to stderr:
/// the `mcp` subcommand speaks the MCP protocol over stdout, and stderr is
/// captured by journald for the daemons all the same.
pub fn init_tracing(default_filter: &str) {
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .init();
}
