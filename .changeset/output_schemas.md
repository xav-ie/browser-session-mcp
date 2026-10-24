---
default: minor
---

# Tools declare the shape of their JSON results

The 19 tools that return structured JSON declare it as an MCP `outputSchema`, generated from the same Rust types that produce the result, so the two can't drift. Clients can validate results and generate types from `tools/list`.

Tool descriptions also say what the text-only tools return, where `stop_profiling` saves its profile, and that `start_profiling` fails on a tab that is already being profiled.
