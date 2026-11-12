---
default: patch
---

# Radio questions and upload widgets by plain language

- `select_option` lists and picks options in radio groups, clicking through
  the label when the radio itself is hidden, and checks it took.
- Radios and checkboxes are described with their group's question, and
  `select_option` counts a radio group's buttons as one answer.
- A see-through or sliver-thin file input (FilePond) is offered as the widget
  drawn over it.
- A selector is looked for in all frames at once.
- `tests/scenarios` covers radio groups and FilePond-like widgets.
