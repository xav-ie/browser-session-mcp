# Deployment

_[← Back to README](../README.md) · [Docs index](README.md)_

The [Quick start](../README.md#quick-start) covers the prebuilt-binary path.
This page covers building from source, the full env-var reference, and running
the supporting daemons in production.

- [Building from source (Nix)](#building-from-source-nix)
- [NixOS module](#nixos-module)
- [Running the daemons without Nix (systemd)](#running-the-daemons-without-nix-systemd)
- [Environment](#environment)
- [Operational notes](#operational-notes)

## Building from source (Nix)

Build with Nix (produces the `browser-session` binary, wrapped so its `takeover`
role finds the bundled Astro UI):

```sh
nix build           # → ./result/bin/browser-session   (subcommands: mcp|listener|reaper|takeover)
nix develop         # dev shell with cargo, rustc, node/pnpm for the frontend
```

The MCP server talks MCP over stdio and expects a Chrome exposing the DevTools
Protocol at `BROWSER_URL` (e.g. `chrome-headless-shell --remote-debugging-port=9222`,
or `https://chrome.<domain>` behind a TLS proxy — TLS is supported):

```sh
BROWSER_URL=http://localhost:9222 browser-session mcp
```

Without the Nix wrapper, point the takeover daemon at the bundled `webroot/`
yourself:

```sh
TAKEOVER_WEBROOT=$PWD/webroot CHROME_WS_BASE=ws://localhost:9222 \
  ./browser-session takeover
```

## NixOS module

The flake exports `nixosModules.default`, which runs the host-side stack — a
persistent headless Chrome plus the `listener`, `reaper` and `takeover` daemons —
as systemd units sharing one state dir. (The MCP server itself is a stdio
subprocess your MCP client/proxy spawns; it is not a daemon, so the module does
not manage it — point it at the same `stateDir` and `browserUrl`.)

```nix
{
  inputs.browser-session-mcp.url = "github:xav-ie/browser-session-mcp";

  # in your NixOS configuration:
  imports = [ inputs.browser-session-mcp.nixosModules.default ];

  services.browser-session = {
    enable = true;
    chrome.package = pkgs.chrome-headless-shell; # or chromium (+ chrome.executable)
    # Required once takeover is enabled — the public CDP WebSocket base the
    # takeover page connects to directly:
    takeover.chromeWsBase = "wss://chrome.example.com";
    # Optional: real-GPU WebGL, idle policy, external Chrome, etc.
    # chrome.extraArgs = [ "--use-gl=angle" "--use-angle=vulkan" ];
    # reaper.maxIdleHours = 2;
  };
}
```

`package` defaults to this flake's build for the host's system. Key options:
`stateDir`, `browserUrl`, `chrome.{enable,port,dataDir,extraArgs,environment}`,
`listener.enable`, `reaper.{interval,maxIdleHours}`, `takeover.{address,port}`.
Reverse-proxy/TLS routing to Chrome and the takeover daemon is left to you.

## Running the daemons without Nix (systemd)

The release tarball bundles a `systemd/` directory: units for the `listener`,
`reaper` (+ timer) and `takeover` roles, plus a `browser-session.env` you point
them at. See [`packaging/systemd/README.md`](../packaging/systemd/README.md) for the
install steps (drop the binary in `/usr/local/bin`, the units in
`/etc/systemd/system`, edit the env file, `systemctl enable --now`).

## Environment

`browser-session mcp` (the MCP server):

| var | default | notes |
|---|---|---|
| `BROWSER_URL` | — (**required**) | DevTools HTTP(S) endpoint |
| `STATE_FILE` | `/var/lib/browser-session-mcp/state.json` | |
| `LOGS_DIR` | `/var/lib/browser-session-mcp/logs` | |
| `STATES_DIR` | `/var/lib/browser-session-mcp/states` | saved cookies |
| `TAKEOVER_DIR` | `/var/lib/browser-session-mcp/takeover` | takeover IPC |
| `TAKEOVER_BASE_URL` | — | public URL of the takeover daemon; without it `request_human_takeover` errors |

> Running the `mcp` role standalone (not under systemd)? The defaults live under
> `/var/lib/browser-session-mcp`, which a non-root user can't write. Point them
> at a writable dir, e.g.
> `STATE_FILE=./bs/state.json LOGS_DIR=./bs/logs STATES_DIR=./bs/states TAKEOVER_DIR=./bs/takeover`.

`browser-session listener` — `BROWSER_URL` (default `http://127.0.0.1:9222`),
`LOGS_DIR`, `TAKEOVER_DIR`. Run as a systemd service with `Restart=always`,
ordered `After=` Chrome.

`browser-session reaper` — `BROWSER_URL`, `STATE_FILE`, `LOGS_DIR`,
`MAX_IDLE_HOURS` (default 24). Run on a timer (e.g. every 12h).

`browser-session takeover` — `TAKEOVER_BIND` (default `127.0.0.1:9223`),
`TAKEOVER_DIR`, `CHROME_WS_BASE` (**required**, e.g. `wss://chrome.<domain>`),
`TAKEOVER_WEBROOT` (set automatically by the Nix wrapper). Run as a systemd
service.

## Operational notes

- The listener is the only source of console + network events. While it's down
  those events are lost; `Restart=always` keeps that window to seconds.
- Sessions survive transport churn and MCP-subprocess restarts, **but not a
  Chrome restart** — if Chrome restarts, all contexts are gone and agents must
  `open_browser_session` again.
- The reaper deletes a session's NDJSON folder when it closes the context, so
  log space stays bounded. Idle cutoff is `MAX_IDLE_HOURS`.
- There's no per-visit log rotation: a single very long-lived visit on a noisy
  SPA can grow its NDJSON file without bound. In practice this hasn't been a
  problem.

---

**Back to** [Quick start](../README.md#quick-start) · [Docs index](README.md)
