---
default: patch
---

# act can tell which carousel slide shows

The page text `act` and plain-language selectors send Jev left in every
slide of a carousel, so "show slide 3" stopped early and "next slide"
overshot. Text scrolled sideways out of view is now left out. A phone task
set for `act` is in `bench/mobile.mjs`, run with `--mobile`.
