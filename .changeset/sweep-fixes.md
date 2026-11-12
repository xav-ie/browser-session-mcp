---
default: patch
---

# Radio questions, upload widgets, and links of closed sessions

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
