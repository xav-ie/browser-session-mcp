---
default: patch
---

# Forms say what they did

- `type` sets date, time, datetime-local, month, week, range and color inputs
  directly, so typed dates no longer leave the field empty. Dates take
  2026-03-14 or 03/14/2026, times 14:30 or 2:30 PM.
- `type` reads the field back: it reports text the page reformatted and fails
  when the field is empty afterwards.
- `click`, `tap`, `press_key` and `select_option` report the fields a page
  rejected during the action: the browser's own messages ("Please fill out
  this field") and error text a page ties to a field it marks invalid.
- `select_option` closes a multi-select's list after the pick.
