# Changelog
## 0.1.1 (2026-10-20)

### Features

#### Install with Homebrew

`brew install xav-ie/tap/browser-session-mcp` installs the binary with the takeover UI on macOS (Apple silicon) and Linux.

### Fixes

#### The macOS binary runs without Nix

It links only macOS's own libraries now, so it runs on any Mac with Apple silicon and macOS 14 or newer.

#### The quick start sets STATE_DIR

The README's `claude mcp add` commands now pass the `STATE_DIR` the server requires, so following them works as written.

## 0.1.0 (2026-10-20)

### Features

#### First release

Isolated, long-lived Chrome sessions for AI agents over MCP: 30 tools for tabs, navigation, trusted input, screenshots, accessibility snapshots and per-visit console and network logs, with anti-bot stealth, human login takeover and saved cookie states. Prebuilt binaries for Linux (x86_64 and aarch64, static) and macOS (Apple silicon, with Nix).
