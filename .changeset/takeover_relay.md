---
default: minor
---

# Takeover relays CDP; Chrome stays on loopback

The takeover page no longer connects to Chrome's DevTools endpoint directly.
The takeover daemon accepts its WebSocket at
`/takeover/<token>/devtools/page/<targetId>` and relays it to Chrome at
`BROWSER_URL`, only with the link's claim cookie, only for tabs in the
session, and only for the CDP methods the page uses. Navigation is limited
to http(s) and about:blank, other sessions' targets are filtered out, and the
socket closes on Done or expiry. Chrome no longer needs a public route or
`--remote-allow-origins=*`.

Breaking for deployments: `CHROME_WS_BASE` and the NixOS option
`takeover.chromeWsBase` are gone (setting the option now fails evaluation
with a pointer here). The takeover daemon reads `BROWSER_URL` instead,
defaulting to `http://127.0.0.1:9222`. The Done button also reports a failed
handback instead of claiming success.
