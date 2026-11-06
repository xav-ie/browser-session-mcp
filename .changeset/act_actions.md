---
default: minor
---

# act can hover, drag, right-click and press any key

Besides clicking and typing, an `act` step can hover an element, right-click
it, drag it onto another (HTML5 drag-and-drop included), or press a key:
Enter, Escape, Tab, Shift+Tab, the arrows, Space, Backspace, Delete, Page
Up/Down, Home or End. It accepts JavaScript dialogs (listed in `dialogs`)
instead of hanging on them, follows a link that opens a new tab, and waits
out a page mid-navigation. It reaches elements in open shadow roots and
same-origin iframes, and elements only a script makes clickable, such as
sortable table headers.
