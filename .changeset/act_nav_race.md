---
default: patch
---

# act survives navigation races

A click that triggers navigation tore the document down before the next
step's DOM extraction ran, failing the whole loop one step from success.
The loop now pauses briefly after each executed action, and extraction
retries a couple of times before giving up.
