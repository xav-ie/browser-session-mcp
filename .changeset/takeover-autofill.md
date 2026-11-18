---
default: patch
---

# Smarter autofill on the takeover page

- The credential popover has an Autofill button: it fills the username and
  password, submits, then fills the one-time code on the next step and submits
  again. Values still reach the page only as typed input. Press it again to
  stop; Enter in a popover box runs it too.
- The popover stays open while you use the page and closes only from its own
  close button or the key button.
- Enter typed on the page submits forms again.
- Sending one value moves past links and toggles (forgot username, show
  password, remember me), up to five Tabs, to the next field or submit button.
