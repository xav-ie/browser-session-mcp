---
default: patch
---

# Jev never sees what's typed into a field

Element descriptions sent to Jev used to fall back to a text field's
current value when it had no label, so a card number or autofilled email
typed into an unlabeled box went to the Jev API on the next step. Text
inputs, textareas and contenteditable editors now never use their contents
as a label; they read (filled) or (empty) instead, which also tells act a
field is already done. Button-type inputs keep their value as their text.
