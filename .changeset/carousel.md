---
default: patch
---

# act swipes across the carousel in view

On a phone session, a plain swipe in `act` went across the middle of the
screen, so it missed a carousel lower on the page. It now goes across the
largest box in view that scrolls sideways, falling back to the middle. A
phone task set for `act` is in `bench/mobile.mjs`, run with `--mobile`.
