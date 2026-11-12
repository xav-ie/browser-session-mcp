---
default: patch
---

# act holds submits, and a shared word isn't a match

- With `confirmIrreversible`, `act` also holds a click whose label says it
  submits, sends, pays, orders, publishes, deletes or signs up (a trash icon
  too), read as the label shows now, so a "Next" button that became "Submit"
  is held. Jev is asked about submitting forms and signing up as well.
- `act`'s retry after a step with no effect only moves to an element in the
  same document and form, never to a page control around the work.
- Plain-language selectors and `act` treat an element that only shares a word
  with the goal as no match: "accept cookies" no longer clicks a footer
  "cookies" link when no consent banner is showing.
