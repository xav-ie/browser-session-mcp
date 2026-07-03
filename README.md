# browser-session-mcp

An MCP server that gives each AI agent its own **isolated, long-lived Chrome
browser session** — with full console + network capture, built-in anti-bot
stealth, and a "hand it to a human" flow for logins the agent must not see.

Sessions are addressed by an id you pass into every tool call, so a session
keeps its cookies and tabs even when the MCP connection drops and reconnects, or
the MCP subprocess is restarted. Written in Rust, driving the Chrome DevTools
Protocol (CDP) via a lightly-forked
[`chromiumoxide`](https://github.com/xav-ie/chromiumoxide).

> **New here?** Jump to [Quick start](#quick-start) — start a Chrome, grab a
> binary, make your first tool call. Then browse the [tool surface](#tool-surface-28-tools).
> Everything deeper lives in [`docs/`](#learn-more).

## Why you'd want it

- **Isolated sessions** — each agent gets its own cookies/storage/tabs; run many
  in parallel against one shared Chrome with zero cross-talk.
- **Survives restarts** — because a session is a tool argument (not a transport
  concept), it outlives transport reconnects and MCP-subprocess restarts (though
  *not* a Chrome restart — see [Operational notes](docs/deployment.md#operational-notes)).
- **Full capture** — every console message and network request is logged to disk
  losslessly, queryable per page-visit.
- **Anti-detection** — drives real sites without tripping bot gates like
  Cloudflare, hiding the usual headless/automation giveaways (no `Runtime.enable`
  signal, coherent User-Agent + Client Hints, `navigator.webdriver` masked, real
  CDP input). Toggle stealth per session.
- **Human takeover** — hand a live page to a person for password/passkey/2FA
  login without the agent ever seeing the credentials.
- **Saved cookie states** — log in once, reuse across future sessions.
- **[28 tools](#tool-surface-28-tools)** covering tabs, navigation, trusted
  input, screenshots, accessibility snapshots, and logs.

## Prerequisites

- **A Chrome exposing the DevTools Protocol (CDP).** This server does not launch
  or bundle Chrome — you point it at a running one. Any Chrome or Chromium works;
  just launch it with `--remote-debugging-port=9222`. Don't have one handy? Grab
  a minimal headless build with
  `npx @puppeteer/browsers install chrome-headless-shell`. (A remote Chrome
  behind a TLS proxy works too.) Without it the server has nothing to drive.
- **Linux or macOS (x86_64 or aarch64)** for the prebuilt binaries. On other
  platforms, [build from source with Nix](docs/deployment.md#building-from-source-nix) (Linux only).
- **An MCP client** (Claude Code, or any MCP-capable agent host).

## Quick start

**1. Start a Chrome exposing CDP:**

```sh
chrome-headless-shell --remote-debugging-port=9222 &
# any Chrome/Chromium works too: google-chrome --remote-debugging-port=9222 &

# sanity check — this should print JSON, not "connection refused":
curl -s http://localhost:9222/json/version
```

**2. Get the `browser-session` binary** — pick one:

<details open>
<summary><b>Nix users</b> — no download, run straight from the flake</summary>

```sh
# Run straight from the flake (the `mcp` role is the MCP server):
BROWSER_URL=http://localhost:9222 nix run github:xav-ie/browser-session-mcp -- mcp
```

The first `nix run` builds/fetches from the binary cache (can take a few
minutes); later runs are instant. Skip to step 3 — for Nix you wire in the
`nix run …` command directly, no absolute path needed. For a dev shell and the
from-source path, see
[Deployment › Building from source](docs/deployment.md#building-from-source-nix).

</details>

<details>
<summary><b>Prebuilt binary</b> — download a release tarball</summary>

Each tagged [release](https://github.com/xav-ie/browser-session-mcp/releases)
ships a tarball per platform containing the single `browser-session` binary plus
the built takeover UI. Download, verify, extract:

```sh
ver=v0.1.0                                   # pick a release tag
arch=x86_64-unknown-linux-gnu                # find yours with `uname -sm`:
                                             #   Linux x86_64  → x86_64-unknown-linux-gnu
                                             #   Linux aarch64 → aarch64-unknown-linux-gnu
                                             #   Darwin arm64  → aarch64-apple-darwin
                                             #   Darwin x86_64 → x86_64-apple-darwin
base=https://github.com/xav-ie/browser-session-mcp/releases/download/$ver
curl -fsSLO "$base/browser-session-$ver-$arch.tar.gz"
# verify (macOS: swap `sha256sum -c` for `shasum -a 256 -c`)
curl -fsSL "$base/SHA256SUMS" | grep "browser-session-$ver-$arch.tar.gz" | sha256sum -c -
tar -xzf "browser-session-$ver-$arch.tar.gz"
cd "browser-session-$ver-$arch"              # → the browser-session binary + webroot/
```

</details>

`browser-session` is a single binary that behaves as four different programs
depending on its first argument — `mcp`, `listener`, `reaper`, or `takeover`
(these are its **roles**). You only need `mcp` to get started; the other three
are optional background helpers (see [Deployment](docs/deployment.md)).

**3. Wire it into your MCP client** — e.g. Claude Code:

```sh
# Nix (runs from the flake, no path to manage):
claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -- nix run github:xav-ie/browser-session-mcp -- mcp

# Prebuilt binary (by absolute path):
claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -- /abs/path/to/browser-session mcp
```

(To run it directly instead: `BROWSER_URL=http://localhost:9222 ./browser-session mcp`,
or the `nix run …` line from step 2.)

**4. Drive the browser.** Once it's wired in, you just ask your agent — it calls
these MCP tools for you. Try a prompt like:

> Open a browser session, go to example.com, and take a screenshot.

Under the hood that's a `sessionId` from `open_browser_session`, threaded through
each call. In pseudo-code (tool name + args — `browser_session_mcp` is the MCP
namespace your client assigns this server), the loop is:

```ts
const { sessionId } = (
  await tools.browser_session_mcp.open_browser_session({})
).structuredContent;

await tools.browser_session_mcp.navigate({ sessionId, url: "https://example.com" });
await tools.browser_session_mcp.take_screenshot({ sessionId });
```

That's the whole loop. For login flows and cookie reuse, see
[Workflows](docs/workflows.md). For the full stealth/capture story, see
[How it works](docs/how-it-works.md).

> For lossless console/network capture and idle cleanup, also run the
> `listener` and `reaper` as background daemons — see [Deployment](docs/deployment.md).
> The `mcp` role works standalone without them; you just won't get the
> `list_console_messages` / `list_network_requests` logs.

## Tool surface (28 tools)

Grouped by what they do: **session lifecycle**, **tabs**, **navigation**,
**capture** (screenshot/snapshot), **interaction** (click/type/scroll/eval),
**per-visit logs**, **stealth**, **human takeover**, and **saved cookie states**.

Most tools take `sessionId` as their first argument. The exceptions are the
session lifecycle/listing calls (which create or enumerate sessions) and
`await_human_takeover` (which keys off a `token` instead). Tools marked ⚙ need an
optional daemon from [Deployment](docs/deployment.md); the rest work from the
Quick start setup above.

<details>
<summary><b>Full tool list</b> (click to expand)</summary>

**Session lifecycle**
- `open_browser_session({ viewport?, useMobileUA? })` → `{ sessionId, pageCount, activeUrl }`
- `close_browser_session({ sessionId })` — idempotent
- `list_browser_sessions()`

**Tabs**
- `new_page({ sessionId, url? })` — opens a tab; subsequent calls target it
- `list_pages({ sessionId })` — stable order, active tab marked `*`
- `switch_tab({ sessionId, tabIndex })` — make a tab active + bring to front
- `close_tab({ sessionId, tabIndex })`

**Navigation**
- `navigate({ sessionId, url, timeout? })` — resolves after the load event

**Capture**
- `take_screenshot({ sessionId, fullPage? })` — PNG as base64
- `take_snapshot({ sessionId })` — accessibility tree as indented text

**Interaction** (all via trusted CDP input)
- `click({ sessionId, selector | x,y, timeout? })`
- `type({ sessionId, selector, text, delay?, clear? })`
- `press_key({ sessionId, key, selector?, timeout? })` — `key` is a name (Enter, Tab, ArrowDown…) or a char
- `scroll({ sessionId, deltaY?, deltaX?, x?, y? })`
- `move_mouse({ sessionId, x, y })`
- `wait_for({ sessionId, selector? | text?, timeout? })`
- `evaluate({ sessionId, expression, tabIndex? })` — runs JS, returns the value

**Per-visit logs** ⚙ (read the `listener` daemon's newline-delimited JSON)
- `list_visits({ sessionId })` — visit headers, oldest first
- `list_console_messages({ sessionId, visit?, limit? })` — `limit` default 500
- `list_network_requests({ sessionId, visit?, limit? })` — `limit` default 500

**Stealth**
- `set_stealth({ sessionId, enabled })`
- `get_stealth({ sessionId })` → `{ stealth }`

**Human takeover** ⚙ (login/passkey without the agent seeing credentials; needs the `takeover` daemon + `TAKEOVER_BASE_URL`)
- `request_human_takeover({ sessionId, ttl? })` → `{ url, token, expiresAtMs }` — non-blocking; mints a link
- `await_human_takeover({ token, timeout? })` → `{ completed }` — blocks until the human clicks Done

**Saved cookie states**
- `save_browser_state({ sessionId, name })`
- `load_browser_state({ sessionId, name })`
- `list_browser_states()`
- `delete_browser_state({ name })`

</details>

## Learn more

Full docs live in [`docs/`](docs/README.md) (indexed in reading order):

- **[Workflows](docs/workflows.md)** — the two multi-call flows: [human
  takeover](docs/workflows.md#human-takeover) (hand a login to a person) and
  [saved cookie state](docs/workflows.md#saved-cookie-state) (log in once, reuse).
- **[How it works](docs/how-it-works.md)** — why [sessions are a tool
  argument](docs/how-it-works.md#sessions-are-a-tool-argument), the
  [four-process architecture](docs/how-it-works.md#architecture), the
  [anti-detection/stealth](docs/how-it-works.md#anti-detection-stealth) story, and
  the on-disk [storage layout](docs/how-it-works.md#storage-layout).
- **[Deployment](docs/deployment.md)** — [building from
  source](docs/deployment.md#building-from-source-nix), the
  [NixOS module](docs/deployment.md#nixos-module),
  [systemd without Nix](docs/deployment.md#running-the-daemons-without-nix-systemd),
  the full [env-var reference](docs/deployment.md#environment), and
  [operational notes](docs/deployment.md#operational-notes).
