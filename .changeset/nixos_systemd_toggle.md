---
default: minor
---

# NixOS module can skip its units

The NixOS module gains `systemd.enable` (default on). Turning it off drops the
Chrome, listener, reaper and takeover units but keeps the `chrome-headless`
user and the state and profile directories, for running the stack in
containers. The new read-only `chrome.command` is the exact Chrome command
line, and the `chrome-headless` unit runs it.
