---
default: minor
---

# MCP over streamable HTTP

`browser-session mcp` serves streamable HTTP at `/mcp` when `MCP_HTTP_BIND` is
set (stateless, JSON responses), so it no longer needs a stdio to HTTP proxy in
front. One process shares its Chrome connection and session state across all
clients. `MCP_HTTP_ALLOWED_HOSTS` lists the accepted `Host` values (loopback
only by default), for running behind a reverse proxy. Without `MCP_HTTP_BIND`
it speaks stdio as before.
