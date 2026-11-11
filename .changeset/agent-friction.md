---
default: minor
---

# Actions say what happened, and fixes from three days of real use

Agents were rebuilding with `evaluate` what the tools should tell them.

- `click`, `tap`, `swipe` and `press_key` report where the page is after:
  URL, title, whether it navigated, the focused element, an open dialog and
  dialogs the page raised. A click or tap on a target a popup or overlay
  covers, or off the screen, fails naming what is in the way instead of
  reporting success, and a hidden element is no longer clicked at (0, 0).
- `press_key` takes `repeat`, listing focus after each press, so a whole
  Tab walk is one call.
- `wait_for` takes `ms` for a plain pause (Executor's sandbox has no timer)
  and `expression` to wait for JavaScript to turn truthy. `navigate` takes
  `waitUntil: "networkidle"` and `text` for pages that render after loading.
- The page's own dialogs (`alert`, `confirm`, `prompt`, "Leave site?") are
  accepted on every tab as they open, so one can no longer hang the tab.
- Passkey prompts are turned down on every tab, so sites fall back to a
  password or a code instead of stalling on a prompt nobody can answer.
- One call drives a session at a time; a second call on the same session,
  from any client, waits for the first.
- `take_screenshot` can `save` to a file on the host, kept after the session
  closes, capture one element with `selector`, and `scale`.
- `open_browser_session({ name })` reuses the open session with that name,
  or opens one with the login saved under it; a named session saves its
  login when a takeover finishes and when it closes.
  `await_human_takeover` waits 25 s by default and reports `expired`, `url`
  and `title`.
- `open_browser_session` takes a `device` preset, a `userAgent` and
  `viewport.deviceScaleFactor`, and phones now report their screen size.
- `list_network_requests` takes `details` for request and response headers
  and small response bodies (credentials masked, nothing from a takeover)
  and `urlContains`; large request bodies are kept, and a finished request no
  longer loses its status or headers to event ordering.
- `measure_page` reports how fast a page loads: paints, layout shift,
  blocking time and bytes, rated by the Core Web Vitals thresholds.
- `type` adds to the end of what a field holds instead of where the click
  put the caret.
- Plain-language selectors name web component buttons by their slotted text,
  so buttons whose text sits in a `<slot>` are found.
- `evaluate` errors carry the thrown error's message, not just "Uncaught",
  and a covering element inside a web component names its host on the page.
- The takeover page's fill panel also fills a one-time code, and every fill
  presses Tab after.
- `tests/scenarios` replays these situations on local pages that reproduce
  them, run in CI beside the smoke test.
