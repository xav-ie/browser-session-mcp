# Changelog
## 0.1.14 (2026-10-30)

### Features

#### MCP over streamable HTTP

`browser-session mcp` serves streamable HTTP at `/mcp` when `MCP_HTTP_BIND` is
set (stateless, JSON responses), so it no longer needs a stdio to HTTP proxy in
front. One process shares its Chrome connection and session state across all
clients. `MCP_HTTP_ALLOWED_HOSTS` lists the accepted `Host` values (loopback
only by default), for running behind a reverse proxy. Without `MCP_HTTP_BIND`
it speaks stdio as before.

#### NixOS module can skip its units

The NixOS module gains `systemd.enable` (default on). Turning it off drops the
Chrome, listener, reaper and takeover units but keeps the `chrome-headless`
user and the state and profile directories, for running the stack in
containers. The new read-only `chrome.command` is the exact Chrome command
line, and the `chrome-headless` unit runs it.

### Fixes

#### Jev never sees what's typed into a field

Element descriptions sent to Jev used to fall back to a text field's
current value when it had no label, so a card number or autofilled email
typed into an unlabeled box went to the Jev API on the next step. Text
inputs, textareas and contenteditable editors now never use their contents
as a label; they read (filled) or (empty) instead, which also tells act a
field is already done. Button-type inputs keep their value as their text.

## 0.1.13 (2026-10-30)

### Features

#### Takeover relays CDP; Chrome stays on loopback

The takeover page no longer connects to Chrome's DevTools endpoint directly.
The takeover daemon accepts its WebSocket at
`/takeover/<token>/devtools/page/<targetId>` and relays it to Chrome at
`BROWSER_URL`, only with the link's claim cookie, only for tabs in the
session, and only for the CDP methods the page uses. Navigation is limited
to http(s) and about:blank, other sessions' targets are filtered out, and the
socket closes on Done or expiry. Chrome no longer needs a public route or
`--remote-allow-origins=*`.

Breaking for deployments: `CHROME_WS_BASE` and the NixOS option
`takeover.chromeWsBase` are gone (setting the option now fails evaluation
with a pointer here). The takeover daemon reads `BROWSER_URL` instead,
defaulting to `http://127.0.0.1:9222`. The Done button also reports a failed
handback instead of claiming success.

## 0.1.12 (2026-10-30)

### Fixes

#### Jev sees checkbox and toggle state

Extracted elements now carry their state: checkboxes and radios read
(checked) or (unchecked), and aria-checked, aria-pressed, aria-expanded and
aria-selected show up too. Unlabeled checkboxes and radios take the bare
text next to them as their label instead of their form value ("on"). act
can now tell a toggle goal is met instead of clicking the box back off.

## 0.1.11 (2026-10-30)

### Fixes

#### Connection, isolated world and act reliability fixes

Tool calls no longer queue behind the Chrome liveness probe: it runs outside
the shared lock, at most every 2s, and a failed probe replaces only the
connection it found dead. Transport errors are recognized by type (not by
the word "websocket" in a message) and only force a re-probe. Isolated
worlds are reused per document behind a random marker instead of created
per eval, which leaked renderer memory under polling. In act, the effect
check fingerprints visible text and form state, so modals and toggles count
as progress; a run with values no longer stops at login forms; and each
extraction's stamps carry a random token, so a concurrent extraction makes
a stale pick fail closed instead of clicking the wrong element. Unit tests
now run in CI.

## 0.1.10 (2026-10-30)

### Features

#### Jev improvements: page text, effect checks, speculative values

Every Jev decision now sees the page's visible text (modals first), so
goals can reference content that is not a label. The act loop verifies
each action had an effect, retries with Jev's next-best element when one
does not, and validates answer probability distributions. Form steps get
a speculative value-key question in the same round trip. Also: selects
match by option index instead of truncated labels, a back pseudo-action,
login/error walls surface as needs_login/blocked statuses, and empty
pages answer goal-met instead of erroring.

### Fixes

#### act survives navigation races

A click that triggers navigation tore the document down before the next
step's DOM extraction ran, failing the whole loop one step from success.
The loop now pauses briefly after each executed action, and extraction
retries a couple of times before giving up.

## 0.1.9 (2026-10-30)

### Features

#### act: a Jev-driven goal loop

New `act({ sessionId, goal, values?, maxSteps? })` tool. The server extracts
the page's interactive elements, asks Jev for the next step (one fan-out
request also judging goal-met and stuck, gated at 0.85), executes it — click,
type, select, scroll or wait — and repeats until the goal reads achieved,
nothing can progress, or the step budget runs out. Values are typed via CDP;
only their keys reach the model.

### Fixes

#### Security: override postcss-selector-parser

Pins postcss-selector-parser to ^7.1.6 (resolves 7.1.7) via a pnpm
override, clearing the Dependabot moderate alert, with the pnpmDeps
hash repinned for the updated lockfile.

## 0.1.8 (2026-10-30)

### Features

#### Plain-language element selection with Jev

`click`, `type`, and `press_key` selectors accept plain language, resolved by TypeSafe's Jev decision model. A `jev:<goal>` prefix goes straight to it, and with `TYPESAFE_API_KEY` set, a selector that matches nothing as CSS is retried as a plain-language goal once the timeout lapses. Jev picks from the page's interactive elements (viewport-first, capped at 240) and every miss fails closed with the original error. `JEV_FALLBACK_MIN_CONFIDENCE` (0.85) guards the automatic path and `JEV_MIN_CONFIDENCE` (0.5) the `jev:` one.

### Fixes

#### Dependencies refreshed

A full `cargo update` — rustls 0.23.46, quinn-proto 0.11.19, getrandom 0.4 among them — clears both Rust security alerts, and pnpm overrides (`fast-uri` 3.1.9, `brace-expansion` 5.0.12) clear all twelve npm alerts. Also folds in Dependabot's schemars/syn and getrandom bumps.

#### wss TLS restored

A dependency update had left the crate's direct `async-tungstenite` on 0.35 while `chromiumoxide` still resolved 0.32, and cargo features never unify across versions, so browser-level `wss://` attaches failed with "TLS support not compiled in". The direct dependency is pinned back to 0.32 with a comment tying it to `chromiumoxide`'s line.

## 0.1.7 (2026-10-24)

### Fixes

#### Shorter tool descriptions

Tools with an output schema no longer repeat their result shape in the description, so agents load fewer tokens per tool list. `start_profiling` and `stop_profiling` explain when to change `samplingIntervalUs`, `top` and `timeout`.

## 0.1.6 (2026-10-24)

### Fixes

#### Tool descriptions cover edge cases and alternatives

Every tool description now says when not to use it and what to use instead, how its arguments interact, and what happens in edge cases: off-screen coordinates, typing into a non-editable element, a failed `new_page` URL, closing the last tab, and the logs and profiles deleted with a session.

## 0.1.5 (2026-10-24)

### Features

#### Tools declare the shape of their JSON results

The 19 tools that return structured JSON declare it as an MCP `outputSchema`, generated from the same Rust types that produce the result, so the two can't drift. Clients can validate results and generate types from `tools/list`.

Tool descriptions also say what the text-only tools return, where `stop_profiling` saves its profile, and that `start_profiling` fails on a tab that is already being profiled.

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
