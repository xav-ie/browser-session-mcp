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

Set `MCP_HTTP_BIND` to serve streamable HTTP at `/mcp` instead (stateless, JSON
responses; one process shares its Chrome connection and session state across
every client). Behind a reverse proxy, list the public hostname in
`MCP_HTTP_ALLOWED_HOSTS`, since only loopback `Host` headers are accepted by
default:

```sh
BROWSER_URL=http://localhost:9222 MCP_HTTP_BIND=127.0.0.1:9224 browser-session mcp
```

The HTTP port has no auth of its own, and the tools drive a logged-in browser.
When anything besides your MCP client can reach it, set `MCP_HTTP_TOKEN` and
have the client send `Authorization: Bearer <token>`; other requests get 401.

Without the Nix wrapper, point the takeover daemon at the bundled `webroot/`
yourself:

```sh
TAKEOVER_WEBROOT=$PWD/webroot BROWSER_URL=http://localhost:9222 \
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
    chrome.package = pkgs.google-chrome; # unfree; or chrome-headless-shell
    chrome.executable = "google-chrome-stable";
    chrome.extraArgs = [ "--headless" ];
    # Optional: real-GPU WebGL, idle policy, external Chrome, etc.
    # chrome.extraArgs = [ "--headless" "--use-gl=angle" "--use-angle=vulkan" ];
    # reaper.maxIdleHours = 2;
  };
}
```

`package` defaults to this flake's build for the host's system. Key options:
`stateDir`, `browserUrl`, `chrome.{enable,port,dataDir,extraArgs,environment}`,
`listener.enable`, `reaper.{interval,maxIdleHours}`, `takeover.{address,port}`.
Reverse-proxy/TLS routing to the takeover daemon is left to you.

To run the processes yourself (e.g. Chrome and the daemons in containers), set
`systemd.enable = false`. The module then defines no units but still creates the
`chrome-headless` user and the state and profile directories, and the read-only
`chrome.command` holds the exact Chrome command line the unit would have run.

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
| `STATE_DIR` | — (**required**) | shared by every role: `sessions/`, `logs/`, `states/` (saved cookies), `takeover/` (takeover IPC) |
| `MCP_HTTP_BIND` | — | serve streamable HTTP at `/mcp` on this address (e.g. `0.0.0.0:9224`) instead of stdio |
| `MCP_HTTP_ALLOWED_HOSTS` | loopback | comma separated `Host` values the HTTP mode accepts (e.g. `mcp.example.com`) |
| `MCP_HTTP_TOKEN` | — | bearer token every HTTP request must carry; unset accepts any request |
| `TAKEOVER_BASE_URL` | — | public URL of the takeover daemon; without it `request_human_takeover` errors |
| `TYPESAFE_API_KEY` | — | enables Jev element picking on click/type/press_key (`jev:` goals plus the automatic fallback that retries selectors matching nothing as CSS) and the `act` goal loop; without it, plain CSS only |
| `JEV_MODEL` | `jev-1.14.0` | pinned Jev model; upgrade deliberately, don't ride `jev-latest` |
| `JEV_BASE_URL` | `https://api.typesafe.ai` | any `/v1/systemone`-compatible carrier (Vercel AI Gateway, OpenRouter, Cloudflare) |
| `JEV_TIMEOUT_MS` | `10000` | per-request timeout for Jev calls |
| `JEV_MIN_CONFIDENCE` | `0.5` | below this confidence a `jev:` resolution errors instead of acting |
| `JEV_FALLBACK_MIN_CONFIDENCE` | `0.85` | confidence floor for the automatic fallback (a selector that matched nothing as CSS retried as a plain-language goal); stricter than `JEV_MIN_CONFIDENCE` because the caller asked for CSS, not a Jev best guess |

> Every role must get the same `STATE_DIR`; they cooperate through files in it.
> Running standalone, point it at any writable dir, e.g. `STATE_DIR=./bs`. The
> old `STATE_FILE`, `LOGS_DIR`, `STATES_DIR` and `TAKEOVER_DIR` are rejected at
> startup so a half-migrated config can't silently split state.

`browser-session listener` — `BROWSER_URL` (default `http://127.0.0.1:9222`),
`STATE_DIR`. Run as a systemd service with `Restart=always`,
ordered `After=` Chrome.

`browser-session reaper` — `BROWSER_URL`, `STATE_DIR`,
`MAX_IDLE_HOURS` (default 24), `REAP_ORPHAN_TABS` (default off; set to `1` to
also close every tab in Chrome's default context, i.e. tabs opened outside the
MCP. Only do this on a dedicated automation Chrome, never one a human browses).
Run on a timer (e.g. every 30min).

`browser-session takeover` — `TAKEOVER_BIND` (default `127.0.0.1:9223`),
`STATE_DIR`, `BROWSER_URL` (default `http://127.0.0.1:9222`; the Chrome it
relays the takeover page's CDP socket to),
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
