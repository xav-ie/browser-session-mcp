---
default: minor
---

# Act inside forms embedded from another site

- `click`, `tap`, `type`, `select_option`, `upload_file` and plain-language
  selectors reach into cross-origin iframes, in the tab's process or their
  own.
- `take_snapshot` includes each frame's tree under its Iframe node.
- A click inside a frame fails when the page around it covers the point.
- Focus in a frame is reported as the field there, not the `<iframe>`.
- `wait_for` finds `text` in frames; `evaluate` and `wait_for` take `frame`.
- Frames with no size, such as tracking pixels, are skipped, and at most 10
  frames from other sites are searched.
- `tests/scenarios` covers an application form embedded from another site.
