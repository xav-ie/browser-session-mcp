---
default: minor
---

# Jev improvements: page text, effect checks, speculative values

Every Jev decision now sees the page's visible text (modals first), so
goals can reference content that is not a label. The act loop verifies
each action had an effect, retries with Jev's next-best element when one
does not, and validates answer probability distributions. Form steps get
a speculative value-key question in the same round trip. Also: selects
match by option index instead of truncated labels, a back pseudo-action,
login/error walls surface as needs_login/blocked statuses, and empty
pages answer goal-met instead of erroring.
