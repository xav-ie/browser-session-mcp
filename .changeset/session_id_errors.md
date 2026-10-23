---
default: patch
---

# get_stealth rejects an unknown session

`get_stealth` errors on a `sessionId` with no open session, as `set_stealth` does, instead of reporting `false`. `close_browser_session`'s schema says an unknown id succeeds, matching what it does.
