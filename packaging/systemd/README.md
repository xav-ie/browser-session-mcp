# systemd units (non-Nix install)

Run the host-side daemons (`listener`, `reaper`, `takeover`) from a release
tarball without Nix. NixOS users should use the flake's
[`nixosModules.default`](../../README.md#nixos-module) instead — it wires all of
this declaratively.

These units cover the three daemons. The MCP server itself is a stdio subprocess
your MCP client/proxy spawns — not a daemon — so it has no unit here. You also
need a **Chrome exposing the DevTools Protocol** running separately (the units
talk to it via `BROWSER_URL`).

## Install

From the extracted release tarball (`browser-session-<ver>-<target>/`):

```sh
# 1. Binary + takeover UI
sudo install -Dm755 browser-session /usr/local/bin/browser-session
sudo cp -r webroot /usr/local/share/browser-session/webroot

# 2. Config (edit BROWSER_URL / CHROME_WS_BASE / paths to taste)
sudo install -Dm644 systemd/browser-session.env /etc/browser-session/browser-session.env
sudo "${EDITOR:-nano}" /etc/browser-session/browser-session.env

# 3. Units
sudo cp systemd/browser-session-*.service systemd/browser-session-*.timer \
  /etc/systemd/system/
sudo systemctl daemon-reload

# 4. Enable
sudo systemctl enable --now browser-session-listener.service
sudo systemctl enable --now browser-session-takeover.service   # if using takeover
sudo systemctl enable --now browser-session-reaper.timer
```

`ExecStart` and the paths in `browser-session.env` assume `/usr/local`; adjust
the units if you install elsewhere. The units create `/var/lib/browser-session`
(via `StateDirectory=`) and run as root by default — the same state dir the MCP
process must read/write, so keep their `STATE_FILE`/`LOGS_DIR`/`TAKEOVER_DIR`
identical to the MCP's env.

## Verify

```sh
systemctl status browser-session-listener.service
curl -fsS "http://127.0.0.1:9223/healthz"   # takeover daemon
systemctl list-timers browser-session-reaper.timer
```
