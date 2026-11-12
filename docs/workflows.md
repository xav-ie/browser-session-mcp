# Workflows

_[← Back to README](../README.md) · [Docs index](README.md)_

Two flows that need more than a single tool call: handing a login to a human,
and reusing cookies across sessions.

- [Human takeover](#human-takeover)
- [Saved cookie state](#saved-cookie-state)

## Human takeover

When a flow needs credentials the agent must not handle (passwords, passkeys),
hand the live page to a human instead of automating the login:

<!-- diagram:takeover -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/takeover-dark.svg">
  <img src="diagrams/takeover.svg" alt="Sequence: the agent calls request_human_takeover and gets a one-time link back, which it shows to a person. The person opens the link; the takeover page streams the live Chrome tab and forwards their input. They log in with their password, passkey or 2FA straight into Chrome, and the agent never sees it. They click Hand back, and the agent's await_human_takeover call returns completed: true. They can keep watching, and Take control pauses the agent again">
</picture>
<!-- /diagram:takeover -->

```ts
const sid = (await tools.browser_session_mcp.open_browser_session({}))
  .structuredContent.sessionId;
await tools.browser_session_mcp.navigate({
  sessionId: sid,
  url: "https://accounts.google.com",
});

// Mint a link and SHOW IT TO THE USER (this call returns immediately).
const { url, token } = (
  await tools.browser_session_mcp.request_human_takeover({ sessionId: sid })
).structuredContent;
// → present `url`; the user opens it, sees a live view of the page, logs in
//   themselves (passkey/password/2FA), and clicks "Hand back". Until then,
//   calls on this session fail with "a person has control".

// Wait for them to finish, ideally in a background task. Each call waits up
// to 25 s by default, under hosts' per-call limits; the person may take
// minutes, so call again until completed (or request a new link if expired):
let r;
do {
  r = (await tools.browser_session_mcp.await_human_takeover({ token })).structuredContent;
} while (!r.completed && !r.expired);
// Now authenticated (r.url says where); continue. A session opened with a
// `name` has already saved this login for next time. If r.watching, the person
// can still see the page and may take control again: a call failing with "a
// person has control" means await the same token again.
await tools.browser_session_mcp.navigate({
  sessionId: sid,
  url: "https://tagmanager.google.com",
});
```

How it works:

1. `request_human_takeover` writes a ticket (sessionId + the active page's CDP
   targetId) to `${STATE_DIR}/takeover/tokens/<token>.json` and returns a link,
   `${TAKEOVER_BASE_URL}/takeover/<token>`.
2. The `browser-session-takeover` daemon serves that page. Its JavaScript opens
   a WebSocket back to the daemon, which relays it to the tab in Chrome (only
   with this link's claim cookie, only for the session's tabs, and only the
   CDP methods the page needs). It streams the live page via
   `Page.startScreencast` and forwards the human's mouse/keyboard as CDP input.
   While the person watches, the relay passes only what shows the page, so
   no click or key reaches Chrome until they take control again. The socket
   closes when the link ends.
3. Chrome's DevTools endpoint stays on loopback, and nothing the human types
   reaches the agent; the daemon only passes CDP messages through.
4. The person starts in control: every tool call on the session, except
   reading logs, fails until they hand back. "Hand back" drops
   `${STATE_DIR}/takeover/released/<token>`, which unblocks
   `await_human_takeover`; "Take control" removes it again.
5. The link ends on "End", when it expires, or a minute after its last open
   page went away; that drops `${STATE_DIR}/takeover/done/<token>`.

Security: the URL is a bearer credential (valid 30 minutes by default, `ttlMinutes` 1 to 60). The first
browser to open it *claims* it via an `HttpOnly` cookie — a leaked URL is
useless once the real user has it, and only the claimant can hand back, take
control or end it.

## Saved cookie state

The easiest way: open the session with a `name`. While it is open, opening the
same name returns it; after it closes (or Chrome restarts), opening the name
starts a new session with the login saved under it. The login is saved when a
takeover finishes and when the session closes.

```ts
const sid = (await tools.browser_session_mcp.open_browser_session({ name: "github" }))
  .structuredContent.sessionId;
// Log in once (a takeover); every later task opens "github" and is in.
```

By hand, for sites where you don't want to log in every session:

<!-- diagram:saved-state -->
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/saved-state-dark.svg">
  <img src="diagrams/saved-state.svg" alt="Session one logs in, by you or by a person through a takeover. save_browser_state with the name github writes the session's cookies to states/github.json, readable only by you. load_browser_state copies them into any new session, so session two starts already logged in">
</picture>
<!-- /diagram:saved-state -->

```ts
// Once: log in (interactively or via takeover), then save the cookies
await tools.browser_session_mcp.save_browser_state({ sessionId: sid, name: "github" });

// Later, in any new session:
const sid2 = (await tools.browser_session_mcp.open_browser_session({}))
  .structuredContent.sessionId;
await tools.browser_session_mcp.load_browser_state({ sessionId: sid2, name: "github" });
await tools.browser_session_mcp.navigate({ sessionId: sid2, url: "https://github.com/foo" });
// already logged in
```

Cookies are saved as JSON under `${STATE_DIR}/states/<name>.json` in plaintext (file
mode 0600, dir 0700). v1 is **cookies-only** — localStorage is not yet
supported, but the on-disk schema reserves an `origins[]` field for forward
compat.

---

**Next:** [How it works](how-it-works.md) — the design behind these flows.
