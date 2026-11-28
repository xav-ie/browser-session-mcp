---
default: minor
---

# Input that looks like a person's

- Clicks, typing, dropdowns and scrolling move like a person's by default, so
  bot detection that watches input sees one: the cursor travels a curved,
  Fitts-timed path from where it last was and holds the button, keys follow
  a typing rhythm (text over 60 characters is pasted), and the page is
  scrolled to targets with the mouse wheel. `humanize: false` on
  `open_browser_session` turns it off for speed.
- `select_option` chooses a native dropdown's option with a click and the
  keyboard, so the page sees trusted input and change events, and says so
  when it had to set the value directly instead.
- Capitals and symbols such as `!` are typed with Shift held, a pressed mouse
  button reports a real mouse's pressure, and a field is focused by the click
  that lands in it.
- An element already in full view is no longer scrolled before a click.
- The window's size and place agree with the screen's, which is now a common
  desktop size rather than the page's.
- A Safari phone preset (`iphone-15`, `iphone-se`, `ipad-air`) stays a touch
  phone when its viewport is applied again.
