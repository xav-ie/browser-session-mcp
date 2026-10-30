---
default: patch
---

# Connection, isolated world and act reliability fixes

Tool calls no longer queue behind the Chrome liveness probe: it runs outside
the shared lock, at most every 2s, and a failed probe replaces only the
connection it found dead. Transport errors are recognized by type (not by
the word "websocket" in a message) and only force a re-probe. Isolated
worlds are reused per document behind a random marker instead of created
per eval, which leaked renderer memory under polling. In act, the effect
check fingerprints visible text and form state, so modals and toggles count
as progress; a run with values no longer stops at login forms; and each
extraction's stamps carry a random token, so a concurrent extraction makes
a stale pick fail closed instead of clicking the wrong element. Unit tests
now run in CI.
