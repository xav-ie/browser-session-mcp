---
default: patch
---

# Picking elements leaves nothing in the page

Plain-language picks and `act` used to stamp a `data-jev-id` attribute on
every element they offered, a change page scripts could notice. The offered
elements are now remembered in the isolated world, where the page can't see
them.
