---
default: minor
---

# Bearer token for MCP over HTTP

`MCP_HTTP_TOKEN` makes the HTTP mode reject any request without
`Authorization: Bearer <token>` (401), so only the intended client can drive
the browser even when other hosts or containers can reach the port. Unset, the
HTTP mode accepts every request as before.
