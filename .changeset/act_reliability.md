---
default: minor
---

# act finishes far more tasks

On Ying-Kai Liao's jev-browser task suite (40 live tasks), `act` went from
11 or 12 right to 29 to 31, within two of jev-browser itself on the same
machine and model.

`act` now types every value in a form, not just the first: keys already typed
are no longer offered again, and a field with no matching guess gets its own
question. It stops as `likely_completed` once Jev rates the goal at least
even odds met, instead of acting past it. When Jev isn't sure of a step, `act` stops with status `uncertain`
and the `candidates` it was weighing instead of erroring, and reports
`likely_completed` when the goal probably is met. It accepts JavaScript
dialogs (listed in `dialogs`) instead of hanging on them, follows links that
open a new tab, can press Enter and Escape, hover, right-click and drag (HTML5 drag-and-drop
included), waits on a page that is still
loading, and picks elements inside open shadow roots, same-origin iframes and
script-only click targets such as sortable table headers. A bot check stops
it with `needs_human`, and the new `confirmIrreversible` option stops before
an order, payment, send or delete with `needs_confirmation`.

Typing (`type` and `act`) now handles text with no key on a US keyboard,
such as accented letters, which used to fail with "Key not found".
`bench/` runs Ying-Kai Liao's jev-browser task suite against the server.
