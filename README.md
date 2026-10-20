# browser-session-mcp

[![CI](https://github.com/xav-ie/browser-session-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/xav-ie/browser-session-mcp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/xav-ie/browser-session-mcp?color=7c4dbb)](https://github.com/xav-ie/browser-session-mcp/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-7c4dbb)](LICENSE)

**Give every AI agent its own real Chrome, one that keeps its logins, records
what the page did, and gets past bot checks.** browser-session-mcp is an MCP
server that hands each agent an isolated, long-lived browser session. When a
site needs a password or passkey, the agent sends you a link, you log in on a
live view of the page, and the agent carries on without ever seeing your
credentials.

<!-- diagram:architecture -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/diagrams/architecture-dark.svg">
  <img src="docs/diagrams/architecture.svg" alt="Your AI agent sends MCP tool calls with a sessionId to the mcp role. The browser-session binary runs as four roles that share one state dir: mcp drives pages, listener watches every tab and records console messages and requests per visit, reaper closes idle sessions on a timer, and takeover serves the page a person opens to log in, with a live view and input to Chrome. All four talk to one Chrome over the DevTools Protocol on port 9222, which holds every session as its own isolated context with its own cookies, storage and tabs">
</picture>
<!-- /diagram:architecture -->

- **A browser per agent.** Each session has its own cookies, storage and tabs.
  Run many agents side by side on one Chrome without them seeing each other.
- **Sessions that stick around.** A session is just an id the agent passes to
  each tool, so it survives dropped connections and server restarts.
- **Hand logins to a human.** For passwords, passkeys and 2FA, the agent sends
  you a link to a live view of the page. You log in, click Done, and the agent
  continues. Your credentials never reach it.
- **Log in once.** Save a session's cookies under a name and load them into any
  future session.
- **Everything recorded.** Every console message and network request is kept on
  disk, grouped by page visit, for the agent to read back while debugging.
- **Gets past bot checks.** No `Runtime.enable` tell, a consistent User-Agent
  and Client Hints, no injected scripts, and real mouse and keyboard input.
  Stealth can be switched on and off per session.
- **[30 tools](#all-30-tools)** for tabs, navigation, clicks and typing,
  screenshots, accessibility snapshots, logs and CPU profiles.

## Quick start

You need **a Chrome with its DevTools port open** (this server drives one, it
doesn't bundle one) and **an MCP client** such as Claude Code.

**1. Start Chrome.** Any Chrome or Chromium works. No Chrome around? Get a
small headless one with `npx @puppeteer/browsers install chrome-headless-shell`.

```sh
chrome-headless-shell --remote-debugging-port=9222 &
curl -s http://localhost:9222/json/version   # prints JSON when it's ready
```

**2. Add the server to your MCP client.** `STATE_DIR` is where it keeps
sessions, logs and saved logins. With Nix, there's nothing to install:

```sh
claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -e STATE_DIR="$HOME/.local/state/browser-session-mcp" \
  -- nix run github:xav-ie/browser-session-mcp -- mcp
```

<details>
<summary><b>With Homebrew</b> (macOS on Apple silicon, or Linux)</summary>

```sh
brew install xav-ie/tap/browser-session-mcp

claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -e STATE_DIR="$HOME/.local/state/browser-session-mcp" \
  -- browser-session mcp
```

</details>

<details>
<summary><b>Without Nix:</b> download a prebuilt binary</summary>

Each [release](https://github.com/xav-ie/browser-session-mcp/releases) has a
tarball per platform with the `browser-session` binary, the takeover page and
systemd units. The Linux binaries are static and run on any distribution; the
macOS one runs on Apple silicon with macOS 14 or newer.

```sh
ver=$(curl -fsSL https://api.github.com/repos/xav-ie/browser-session-mcp/releases/latest | sed -n 's/.*"tag_name": "\(.*\)".*/\1/p')
case "$(uname -sm)" in
  "Linux x86_64")  target=x86_64-unknown-linux-musl ;;
  "Linux aarch64") target=aarch64-unknown-linux-musl ;;
  "Darwin arm64")  target=aarch64-apple-darwin ;;
esac
base=https://github.com/xav-ie/browser-session-mcp/releases/download/$ver
curl -fsSLO "$base/browser-session-$ver-$target.tar.gz"
# check it (on macOS, use shasum -a 256 -c instead of sha256sum -c)
curl -fsSL "$base/SHA256SUMS" | grep "browser-session-$ver-$target.tar.gz" | sha256sum -c -
tar -xzf "browser-session-$ver-$target.tar.gz"

claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -e STATE_DIR="$HOME/.local/state/browser-session-mcp" \
  -- "$PWD/browser-session-$ver-$target/browser-session" mcp
```

</details>

**3. Ask your agent to use it.**

> Open a browser session, go to example.com, and take a screenshot.

The agent calls `open_browser_session`, gets back a `sessionId`, and passes it
to every tool after that:

```ts
const { sessionId } = (await tools.browser_session_mcp.open_browser_session({})).structuredContent;
await tools.browser_session_mcp.navigate({ sessionId, url: "https://example.com" });
await tools.browser_session_mcp.take_screenshot({ sessionId });
```

That's all you need to start. The `mcp` role works on its own. Run the
`listener` too if you want console and network logs, the `reaper` to clean up
idle sessions, and `takeover` for human logins; [Deployment](docs/deployment.md)
shows how.

> [!TIP]
> On Claude Code, install the
> [browser-session plugin](plugin/README.md) for a `browser-session-navigator`
> agent that already knows how to drive these tools.

## Handing a login to a person

When a page needs credentials the agent must not see, it asks for a takeover
link and shows it to you. The page opens a live view of the agent's tab; you
log in there, click Done, and the agent picks up where it left off.

![The takeover page: a bar with back, forward, the page's URL, new tab, a button to fill a username or password, and Done, above a live view of the agent's tab showing GitHub's sign-in form](docs/images/takeover.png)

<!-- diagram:takeover -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/diagrams/takeover-dark.svg">
  <img src="docs/diagrams/takeover.svg" alt="Sequence: the agent calls request_human_takeover and gets a one-time link back, which it shows to a person. The person opens the link; the takeover page streams the live Chrome tab and forwards their input. They log in with their password, passkey or 2FA straight into Chrome, and the agent never sees it. They click Done, and the agent's await_human_takeover call returns completed: true">
</picture>
<!-- /diagram:takeover -->

The link works for one browser only: the first to open it claims it, and it
expires after 30 minutes by default. [Workflows](docs/workflows.md#human-takeover)
has the details and the code.

## All 30 tools

Most tools take a `sessionId` first. Tools marked ⚙ need one of the optional
background roles from [Deployment](docs/deployment.md).

| Group | Tools |
|---|---|
| Sessions | `open_browser_session`, `close_browser_session`, `list_browser_sessions` |
| Tabs | `new_page`, `list_pages`, `switch_tab`, `close_tab` |
| Navigation | `navigate` |
| Seeing the page | `take_screenshot`, `take_snapshot` (the accessibility tree as text) |
| Acting on it | `click`, `type`, `press_key`, `scroll`, `move_mouse`, `wait_for`, `evaluate` |
| Logs ⚙ | `list_visits`, `list_console_messages`, `list_network_requests` |
| Profiling | `start_profiling`, `stop_profiling` |
| Stealth | `set_stealth`, `get_stealth` |
| Human takeover ⚙ | `request_human_takeover`, `await_human_takeover` |
| Saved logins | `save_browser_state`, `load_browser_state`, `list_browser_states`, `delete_browser_state` |

<details>
<summary><b>Every tool with its arguments</b></summary>

**Sessions**
- `open_browser_session({ viewport?, useMobileUA? })` → `{ sessionId, pageCount, activeUrl }`
- `close_browser_session({ sessionId })`: safe to call twice
- `list_browser_sessions()`

**Tabs**
- `new_page({ sessionId, url? })`: opens a tab; later calls act on it
- `list_pages({ sessionId })`: in a stable order, the active tab marked `*`
- `switch_tab({ sessionId, tabIndex })`: makes a tab active and brings it to the front
- `close_tab({ sessionId, tabIndex })`

**Navigation**
- `navigate({ sessionId, url, timeout? })`: returns after the load event

**Seeing the page**
- `take_screenshot({ sessionId, fullPage? })`: a PNG
- `take_snapshot({ sessionId })`: the accessibility tree as indented text

**Acting on it** (real CDP input, not synthetic JavaScript events)
- `click({ sessionId, selector | x,y, timeout? })`
- `type({ sessionId, selector, text, delay?, clear? })`
- `press_key({ sessionId, key, selector?, timeout? })`: `key` is a name (Enter, Tab, ArrowDown…) or a character
- `scroll({ sessionId, deltaY?, deltaX?, x?, y? })`
- `move_mouse({ sessionId, x, y })`
- `wait_for({ sessionId, selector? | text?, timeout? })`
- `evaluate({ sessionId, expression, tabIndex? })`: runs JavaScript and returns the value

**Logs** ⚙ (written by the `listener` role)
- `list_visits({ sessionId })`: one entry per page visit, oldest first
- `list_console_messages({ sessionId, visit?, limit? })`: `limit` defaults to 500
- `list_network_requests({ sessionId, visit?, limit? })`: `limit` defaults to 500

**Profiling** (find the JavaScript behind a slow or hung page)
- `start_profiling({ sessionId, tabIndex?, samplingIntervalUs? })`: starts the V8 CPU profiler
- `stop_profiling({ sessionId, tabIndex?, top?, terminateAfterMs?, timeout? })`: saves a `.cpuprofile` (open it in DevTools or speedscope.app) under `$STATE_DIR/logs/<sessionId>/profiles/` and returns long tasks, the hottest functions and time per script. If the page is hung, its script is stopped after `terminateAfterMs` (default 5000) so the profile can be collected

**Stealth**
- `set_stealth({ sessionId, enabled })`
- `get_stealth({ sessionId })` → `{ stealth }`

**Human takeover** ⚙ (needs the `takeover` role and `TAKEOVER_BASE_URL`)
- `request_human_takeover({ sessionId, ttlMinutes? })` → `{ url, token, expiresAtMs }`: returns right away with a link
- `await_human_takeover({ token, timeout? })` → `{ completed }`: waits until the person clicks Done

**Saved logins**
- `save_browser_state({ sessionId, name })`
- `load_browser_state({ sessionId, name })`
- `list_browser_states()`
- `delete_browser_state({ name })`

</details>

## Learn more

- **[Workflows](docs/workflows.md):** handing a login to a person, and saving
  cookies to reuse.
- **[How it works](docs/how-it-works.md):** why a session outlives its
  connection, the four roles, how stealth works, and what's on disk.
- **[Deployment](docs/deployment.md):** building from source, the NixOS module,
  systemd without Nix, and every environment variable.
- **[Development](docs/development.md):** checks, diagrams and releases.

## License

[MIT](LICENSE)
