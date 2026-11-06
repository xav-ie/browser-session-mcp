---
default: minor
---

# Mobile sessions are touch phones, with tap and swipe

A session opened with `useMobileUA` used to change only the User-Agent, so
pages still saw a mouse and kept their desktop behavior. It now emulates a
phone: mobile device metrics (the page's viewport meta applies) and a touch
screen, so `navigator.maxTouchPoints`, touch events and `(pointer: coarse)`
all say touch, from a tab's first load. The new `tap` and `swipe` tools send
real touch events; `swipe` can start on an element or a point, go a
direction or to a point, and run slow and short to drag partway. On a mobile
session `act` taps instead of clicking, drags with a finger and swipes, and
on any session it can now act (scroll, swipe, press a key) on a page with no
interactive elements. `press_key` documents that it takes combinations such
as `Shift+Tab`.
