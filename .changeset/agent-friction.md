---
default: minor
---

# Actions say what happened

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
