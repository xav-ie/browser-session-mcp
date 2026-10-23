# Changelog
## 0.1.4 (2026-10-23)

### Fixes

#### get_stealth rejects an unknown session

`get_stealth` errors on a `sessionId` with no open session, as `set_stealth` does, instead of reporting `false`. `close_browser_session`'s schema says an unknown id succeeds, matching what it does.

## 0.1.3 (2026-10-23)

### Fixes

#### Tool definitions describe every argument

Every tool argument has a description in its schema, and every tool carries MCP annotations saying whether it only reads, can be destructive, and is safe to repeat. Descriptions say what each tool returns, when to use it over a similar one, and how it fails.

`type` now honors `delay` (milliseconds between characters), which it previously ignored, and takes a `timeout` for its selector like `click` and `press_key`.

## 0.1.2 (2026-10-21)

### Fixes

#### The takeover page has a favicon

The takeover page's browser tab shows the project's turtle logo instead of a blank icon.

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
