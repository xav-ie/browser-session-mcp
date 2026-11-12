# Changelog
## 0.1.22 (2026-11-12)

### Fixes

#### Icon buttons say what they show

- A control with no name, or only an icon font's glyph for one, is described
  with what its markup says the icon shows, as `(icon "trash")`: from an
  image's alt or file name, an SVG sprite's `<use href>`, `data-icon` or
  `data-testid`, or icon library classes (`fa-trash`, `lucide-x`). Snapshots,
  action results and Jev's element list all show it, so plain language can
  pick "the delete button".
- `act` fails when a field it typed a value into is empty afterwards, naming
  the value's key but never its text.

## 0.1.21 (2026-11-12)

### Features

#### Act inside forms embedded from another site

- `click`, `tap`, `type`, `select_option`, `upload_file` and plain-language
  selectors reach into cross-origin iframes, in the tab's process or their
  own.
- `take_snapshot` includes each frame's tree under its Iframe node.
- A click inside a frame fails when the page around it covers the point.
- Focus in a frame is reported as the field there, not the `<iframe>`.
- `wait_for` finds `text` in frames; `evaluate` and `wait_for` take `frame`.
- Frames with no size, such as tracking pixels, are skipped, at most 10
  frames from other sites are searched, and a snapshot reads frames at once,
  waiting at most 2 s for each.
- `tests/scenarios` covers an application form embedded from another site.

### Fixes

#### Find drop zones by what they say

- Plain-language selectors find a drop zone a script listens on with no file
  input, by its class, id or "drop files here" text.
- A picker with a file and a folder input is offered once, and files go to
  the file input.
- A file input parked in the page body is no longer offered as the page.
- `upload_file` tells Jev it wants a file upload field, so "Resume" means
  the upload, not an "Import resume" button beside it.

#### Forms say what they did

- `type` sets date, time, datetime-local, month, week, range and color inputs
  directly, so typed dates no longer leave the field empty. Dates take
  2026-03-14 or 03/14/2026, times 14:30 or 2:30 PM.
- `type` reads the field back: it reports text the page reformatted and fails
  when the field is empty afterwards.
- `click`, `tap`, `press_key` and `select_option` report the fields a page
  rejected during the action: the browser's own messages ("Please fill out
  this field") and error text a page ties to a field it marks invalid.
- `select_option` closes a multi-select's list after the pick.

#### Radio questions, upload widgets, and links of closed sessions

- `select_option` lists and picks options in radio groups, clicking through
  the label when the radio itself is hidden, and checks it took.
- Radios and checkboxes are described with their group's question, and
  `select_option` counts a radio group's buttons as one answer.
- A see-through or sliver-thin file input (FilePond) is offered as the widget
  drawn over it.
- A selector is looked for in all frames at once.
- A takeover link whose browser session closed ends, and its page says so,
  instead of reconnecting to a tab that's gone.
- A click on something the page has disabled or greyed out fails saying so,
  instead of reporting a click that did nothing.
- `tests/scenarios` covers radio groups, FilePond-like widgets, a greyed out
  question and a takeover link outliving its session.

#### Upload files by path from a shared folder

- The NixOS module creates `STATE_DIR/uploads`, writable by every local user
  and cleared of files older than a day. Agents save a file there and pass its
  `path` to `upload_file`, so its bytes never pass through the model.
- `upload_file`'s description names that folder when the server has one, and
  steers agents to `path` over base64 `content`.
- AVIF, HEIC, HEIF, BMP and TIFF images get their own MIME type.

## 0.1.20 (2026-11-12)

### Features

#### Actions say what happened

- `click`, `tap`, `swipe` and `press_key` return the URL, title, focus, any
  open dialog and whether the page navigated.
- A click on a covered or off-screen target fails, naming what's in the way.
- `press_key` takes `repeat`.
- `wait_for` takes `ms` and `expression`.
- `navigate` takes `waitUntil: "networkidle"` and `text`.
- Page dialogs are accepted and passkey prompts declined on every tab.
- One call drives a session at a time.
- `take_screenshot` takes `save`, `selector` and `scale`.
- `open_browser_session` takes `name`, `device`, `userAgent` and
  `viewport.deviceScaleFactor`.
- `await_human_takeover` defaults to 25 s and reports `expired`, `url` and
  `title`.
- `list_network_requests` takes `details` and `urlContains`.
- New `measure_page` tool for load speed.
- `type` appends at the end of the field.
- Plain-language selectors read text in a web component's `<slot>`.
- `evaluate` errors include the thrown message.
- The takeover fill panel fills one-time codes and presses Tab after each fill.
- `tests/scenarios` reproduces these on local pages in CI.

#### Pick from any dropdown and attach files

- New `select_option` tool lists a dropdown's options, or picks one by text
  and confirms the field shows it. Works on native selects, script-drawn
  dropdowns with or without ARIA roles, and dropdowns in frames.
- `click`, `tap` and `press_key` list the options of a dropdown they leave
  open.
- New `upload_file` tool attaches a file from base64 or a server path, or
  drops it on a drop zone.
- A click that reaches only the page background no longer counts as landing
  on its target.
- `tests/scenarios` covers both tools on local forms and dropdowns.

#### Watch the agent after a takeover, and take control again

- A takeover link now opens with the person in control, and every call on
  the session except reading logs fails with "a person has control" until
  they hand back, so nothing the agent does lands under them and nothing
  reads what they type.
- Done is now Hand back: the agent resumes and the link stays open in watch
  mode, a live view where clicks and keys are refused by the relay. Take
  control pauses the agent again. End closes the link, as does leaving the
  page closed for a minute.
- `await_human_takeover` returns on the handback with a new `watching` field,
  and fails on a token whose link ended and was already reported.
- While watching, a click, tap, key, scroll, touch drag or paste shows a
  hint to take control first.
- The takeover page reconnects by itself after a dropped connection, such as
  a phone pausing it.

### Fixes

#### Find form fields by their question

- Plain-language selectors find a field by the question beside it when the
  page links no label to it, and read `aria-describedby`.
- A hidden file input is offered as the drop zone or box around it.
- A label is no longer offered beside its own control.
- `upload_file` waits up to 2 s for the page to show the file name, in any
  case.
- `tests/scenarios` covers fields laid out as job boards draw them.

#### Takeover wording leads with the link

- `request_human_takeover` opens with "Get a human takeover link (URL)", so
  hosts that show only a description's first line rank it for "takeover link".
- Agents with no background task are told to show the link, end their turn,
  and await once the user says they're done.

## 0.1.19 (2026-11-06)

### Fixes

#### act swipes across the carousel in view

On a phone session, a plain swipe in `act` went across the middle of the
screen, so it missed a carousel lower on the page. It now goes across the
largest box in view that scrolls sideways, falling back to the middle. A
phone task set for `act` is in `bench/mobile.mjs`, run with `--mobile`.

## 0.1.18 (2026-11-06)

### Features

#### Mobile sessions are touch phones, with tap and swipe

A session opened with `useMobileUA` used to change only the User-Agent, so
pages still saw a mouse and kept their desktop behavior. It now emulates a
phone: mobile device metrics (the page's viewport meta applies) and a touch
screen, so `navigator.maxTouchPoints`, touch events and `(pointer: coarse)`
all say touch, from a tab's first load. The new `tap` and `swipe` tools send
real touch events; `swipe` can start on an element or a point, go a
direction or to a point, and run slow and short to drag partway. On a mobile
session `act` taps instead of clicking, drags with a finger and swipes, and
on any session it can now act (scroll, swipe, press a key) on a page with no
interactive elements. `press_key` documents that it takes combinations such
as `Shift+Tab`.

## 0.1.17 (2026-11-06)

### Features

#### act can hover, drag, right-click and press any key

Besides clicking and typing, an `act` step can hover an element, right-click
it, drag it onto another (HTML5 drag-and-drop included), or press a key:
Enter, Escape, Tab, Shift+Tab, the arrows, Space, Backspace, Delete, Page
Up/Down, Home or End. It accepts JavaScript dialogs (listed in `dialogs`)
instead of hanging on them, follows a link that opens a new tab, and waits
out a page mid-navigation. It reaches elements in open shadow roots and
same-origin iframes, and elements only a script makes clickable, such as
sortable table headers.

#### act finishes far more tasks, and says how sure it is

On jev-browser's 40-task suite, `act` went from about 12 tasks right to
about 34. On two held-out sets it was never tuned on, it trails jev-browser
on the first and matches it on the second (see `docs/jev.md`). `bench/`
reruns all three.

It types every value a form needs, asking which value belongs in each
field, and clears a field the way a person does, so React inputs no longer
get text appended. Each decision reads the page's main content and heading
rather than its menus. New statuses report what happened: `likely_completed`
when Jev rates the goal probably met (and the run stops there rather than
acting past it), `uncertain` with the `candidates` Jev was weighing instead
of an error, `needs_human` on a CAPTCHA or bot check, and, with the new
`confirmIrreversible` option, `needs_confirmation` before an order, payment,
send or delete.

#### ask: questions about the page in one cheap call

The new `ask` tool asks Jev about the active tab as it is now, judged on its
URL, title and visible text. Without `options` it answers yes or no with the
probability of yes ("Is an error message showing?"); with `options` it picks
the one that fits and gives each one's probability ("Which page is this?":
product page, cart, checkout). It lets a script branch on what a page shows
without a large model, and checks what `act` did. Needs `TYPESAFE_API_KEY`.

#### Plain-language picks reach every element, and start sooner

A pick used to see at most 240 elements, so a target further down a long
page could never be chosen. Jev now first sees what a person would (dialog
and viewport first) with the option to say the right element isn't there;
only then is the whole page offered, in even groups under its 255-option
limit. Jev can also answer "none of these", which is an error, not a guess.
Selectors that are plain words, like "the login button", go to Jev after one
CSS check instead of waiting out the whole `timeout`.

#### read: only the parts of a page that answer a question

The new `read` tool returns just the passages of the active tab that help
answer a question, under their headings, instead of the whole page. Jev
rates every passage in parallel requests, so a 118,000-character Wikipedia
article comes back as about 12,000 characters in under half a second.
Navigation, sidebars, headers and footers are left out, and a page under
8,000 characters comes back whole without asking Jev. Needs
`TYPESAFE_API_KEY`.

### Fixes

#### Picking elements leaves nothing in the page

Plain-language picks and `act` used to stamp a `data-jev-id` attribute on
every element they offered, a change page scripts could notice. The offered
elements are now remembered in the isolated world, where the page can't see
them.

#### Typing accented text, and key combinations

`type` (and `act`) used to fail with "Key not found" on text with no key on
a US keyboard, such as accented letters; that text is now inserted the way
an input method does it. `press_key` accepts combinations such as
`Shift+Tab` or `Control+a`.

## 0.1.16 (2026-11-04)

### Features

#### Network logs include cross-site iframes and workers

`list_network_requests` now shows requests made by a tab's cross-site iframes
and its workers, logged under the tab's current visit. These run as separate
DevTools targets and were missing before, which hid sandboxed third-party code
such as a checkout page's analytics scripts. The iframe's own document request is
still not logged, since Chrome sends it before the iframe can be attached.

#### Throttle a session's CPU and network, and run init scripts

`set_emulation` slows a session's CPU (`cpuThrottlingRate`) and network (a
DevTools preset such as `slow4g`, or custom latency and bandwidth), and
`add_init_script` runs JavaScript before a page's own scripts on every load,
so agents can measure a page the way a slow phone sees it from the first
byte. Both apply to every tab of the session, including ones opened later,
and are in place when the tool returns; `get_emulation` and
`remove_init_script` read and undo them. They need the listener role.

## 0.1.15 (2026-10-30)

### Features

#### Bearer token for MCP over HTTP

`MCP_HTTP_TOKEN` makes the HTTP mode reject any request without
`Authorization: Bearer <token>` (401), so only the intended client can drive
the browser even when other hosts or containers can reach the port. Unset, the
HTTP mode accepts every request as before.

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
