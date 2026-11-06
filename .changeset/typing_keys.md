---
default: patch
---

# Typing accented text, and key combinations

`type` (and `act`) used to fail with "Key not found" on text with no key on
a US keyboard, such as accented letters; that text is now inserted the way
an input method does it. `press_key` accepts combinations such as
`Shift+Tab` or `Control+a`.
