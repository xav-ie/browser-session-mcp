---
default: patch
---

# Jev sees checkbox and toggle state

Extracted elements now carry their state: checkboxes and radios read
(checked) or (unchecked), and aria-checked, aria-pressed, aria-expanded and
aria-selected show up too. Unlabeled checkboxes and radios take the bare
text next to them as their label instead of their form value ("on"). act
can now tell a toggle goal is met instead of clicking the box back off.
