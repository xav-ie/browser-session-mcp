---
default: patch
---

# Icon buttons say what they show

- A control with no name, or only an icon font's glyph for one, is described
  with what its markup says the icon shows, as `(icon "trash")`: from an
  image's alt or file name, an SVG sprite's `<use href>`, `data-icon` or
  `data-testid`, or icon library classes (`fa-trash`, `lucide-x`). Snapshots,
  action results and Jev's element list all show it, so plain language can
  pick "the delete button".
- `act` fails when a field it typed a value into is empty afterwards, naming
  the value's key but never its text.
