---
default: minor
---

# Guest grants: drive a tab group in someone's own Chrome

- `request_guest_grant` returns a link a person pastes into a new unpacked
  Chrome extension (`extension/`). Once they accept, `navigate`,
  `take_snapshot`, `take_screenshot`, `click` and `type` work on their tabs
  with the `guest-…` session id, and `revoke_guest_grant` ends it.
- Their logins never leave their machine, and the extension enforces every
  limit: a CDP allowlist with no scripts or cookies, the grant's URL patterns,
  a hard denylist of sign-in, security, token and billing pages, and in ask
  mode their approval before every click and Enter.
- The takeover daemon relays it; nothing else to configure.
