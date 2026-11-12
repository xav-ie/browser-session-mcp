---
default: patch
---

# Find drop zones by what they say

- Plain-language selectors find a drop zone a script listens on with no file
  input, by its class, id or "drop files here" text.
- A picker with a file and a folder input is offered once, and files go to
  the file input.
- A file input parked in the page body is no longer offered as the page.
