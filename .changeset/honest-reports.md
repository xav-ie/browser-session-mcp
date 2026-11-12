---
default: patch
---

# Reports that match the page

- After `type`, the next action reports when the page changed the field once
  focus left it, such as a date field that reverts or reformats on blur.
- `type` with `clear` uses the page's own select all and delete (Control+A,
  Backspace) when what has focus isn't the field as a plain input: a rich
  text editor, or a date field whose visible boxes stand in front of a hidden
  input, which used to lose both the old and the new text.
- `rejected` also covers fields in same-origin frames and fields a page marks
  only with a class such as `invalid` or `is-invalid`.
- A click on a shape inside an icon names the control around it, with its icon
  hint, instead of `path` or `line`.
- A target parked off the page to hide it (a "Skip to content" link) is
  reported as such, instead of advice to scroll it into view.
- The `dialog` field gives a dialog's name or heading, and cuts its text at a
  word. A plain-language selector that matches nothing as CSS no longer says
  it timed out, and `select_option` on something that isn't a dropdown says
  to use click or type.
