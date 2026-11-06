---
default: minor
---

# act finishes far more tasks, and says how sure it is

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
