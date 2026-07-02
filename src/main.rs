//! browser-session — a single multi-call binary for the whole stack.
//!
//! Dispatches on the first argument to one of four long-lived roles that
//! cooperate around one Chrome (see the README architecture diagram):
//!
//!   browser-session mcp        MCP server over stdio (the tool surface)
//!   browser-session listener   always-on CDP console/network capture → NDJSON
//!   browser-session reaper     one-shot sweep of idle contexts (run on a timer)
//!   browser-session takeover   HTTP daemon serving the human-takeover page
//!
//! Folding them into one binary keeps the shared async/TLS/CDP runtime compiled
//! once and gives non-Nix users a single executable to place on PATH.
use anyhow::Result;

const USAGE: &str = "\
browser-session — isolated browser sessions over a shared Chrome

Usage: browser-session <command>

Commands:
  mcp         MCP server over stdio (needs BROWSER_URL)
  listener    always-on console + network capture to NDJSON
  reaper      one-shot sweep of idle sessions (run on a timer)
  takeover    HTTP daemon serving the human-takeover page

Each command is configured by environment variables; see the README.";

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("mcp") => browser_session_mcp::mcp::run().await,
        Some("listener") => browser_session_mcp::listener::run().await,
        Some("reaper") => browser_session_mcp::reaper::run().await,
        Some("takeover") => browser_session_mcp::takeover::run().await,
        Some("-V" | "--version") => {
            println!("browser-session {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        other => {
            if let Some(cmd) = other {
                eprintln!("unknown command: {cmd}\n");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}
