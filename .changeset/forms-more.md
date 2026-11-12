---
default: patch
---

# Find form fields by their question

- Plain-language selectors find a field by the question beside it when the
  page links no label to it, and read `aria-describedby`.
- A hidden file input is offered as the drop zone or box around it.
- A label is no longer offered beside its own control.
- `upload_file` waits up to 2 s for the page to show the file name, in any
  case.
- `tests/scenarios` covers fields laid out as job boards draw them.
