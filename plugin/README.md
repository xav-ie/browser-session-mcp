# browser-session (Claude Code plugin)

Ships the `browser-session-navigator` agent that knows how to drive the
**browser-session** MCP — so the agent's playbook always moves in lockstep with
the [28-tool surface](../README.md#tool-surface-28-tools) it targets.

The plugin deliberately does **not** bundle an `.mcp.json`. How you run the MCP
server is your call — connect the `browser-session` MCP to your client yourself
(see the [Quick start](../README.md#quick-start), or point your client at a
proxy). The agent just calls the tools by name once the server is connected.

## Install

This repo _is_ a Claude Code marketplace. Add it, then install the plugin:

```sh
/plugin marketplace add xav-ie/browser-session-mcp
/plugin install browser-session@browser-session-mcp
```

## What you get

- **Agent** — [`browser-session-navigator`](agents/browser-session-navigator.md):
  opens/reuses a `sessionId`, reads pages via accessibility snapshots, acts by
  CSS selector, inspects per-visit console/network logs, and hands logins to a
  human via takeover.

## Wiring the MCP yourself

The agent is inert without the `browser-session` MCP connected to your client.
For example, with Claude Code and a Chrome exposing CDP at
`http://localhost:9222`:

```sh
claude mcp add browser-session -e BROWSER_URL=http://localhost:9222 \
  -- nix run github:xav-ie/browser-session-mcp -- mcp
```

See the top-level [README](../README.md#quick-start) for the binary/proxy
alternatives.
