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
- `tests/scenarios` covers radio groups, FilePond-like widgets and a
  takeover link outliving its session.
