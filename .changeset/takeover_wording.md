---
default: patch
---

# Human takeover guidance leads with the link and polls once a minute

`request_human_takeover` now opens with "Get a human takeover link (URL)", so clients that show only a description's first line, and agents searching for a takeover link, find it ahead of `await_human_takeover`.

The takeover guidance in the tool descriptions, the `request_human_takeover` result and the server instructions now suggests a timeout of about 60000ms per `await_human_takeover` call instead of 4000ms, so a poller makes one call per minute rather than one every few seconds. Agents without a background task are told to show the link, end their turn, and wait once the user says they're done.
