---
default: minor
---

# Plain-language element selection with Jev

`click`, `type`, and `press_key` selectors accept plain language, resolved by TypeSafe's Jev decision model. A `jev:<goal>` prefix goes straight to it, and with `TYPESAFE_API_KEY` set, a selector that matches nothing as CSS is retried as a plain-language goal once the timeout lapses. Jev picks from the page's interactive elements (viewport-first, capped at 240) and every miss fails closed with the original error. `JEV_FALLBACK_MIN_CONFIDENCE` (0.85) guards the automatic path and `JEV_MIN_CONFIDENCE` (0.5) the `jev:` one.
