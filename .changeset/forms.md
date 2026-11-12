---
default: minor
---

# Pick from any dropdown and attach files

- New `select_option` tool lists a dropdown's options, or picks one by text
  and confirms the field shows it. Works on native selects, script-drawn
  dropdowns with or without ARIA roles, and dropdowns in frames.
- `click`, `tap` and `press_key` list the options of a dropdown they leave
  open.
- New `upload_file` tool attaches a file from base64 or a server path, or
  drops it on a drop zone.
- A click that reaches only the page background no longer counts as landing
  on its target.
- `tests/scenarios` covers both tools on local forms and dropdowns.
