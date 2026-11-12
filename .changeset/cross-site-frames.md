---
default: minor
---

# Act inside forms embedded from another site

- `click`, `tap`, `type`, `select_option`, `upload_file` and plain-language
  selectors reach into cross-origin iframes, in the tab's process or their
  own.
- `take_snapshot` includes each frame's tree under its Iframe node.
- A click inside a frame fails when the page around it covers the point.
- `tests/scenarios` covers an application form embedded from another site.
