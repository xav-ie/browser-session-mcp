---
default: patch
---

# Tool definitions describe every argument

Every tool argument has a description in its schema, and every tool carries MCP annotations saying whether it only reads, can be destructive, and is safe to repeat. Descriptions say what each tool returns, when to use it over a similar one, and how it fails.

`type` now honors `delay` (milliseconds between characters), which it previously ignored, and takes a `timeout` for its selector like `click` and `press_key`.
