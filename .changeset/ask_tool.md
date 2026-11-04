---
default: minor
---

# ask: questions about the page in one cheap call

The new `ask` tool asks Jev about the active tab as it is now, judged on its
URL, title and visible text. Without `options` it answers yes or no with the
probability of yes ("Is an error message showing?"); with `options` it picks
the one that fits and gives each one's probability ("Which page is this?":
product page, cart, checkout). It lets a script branch on what a page shows
without a large model, and checks what `act` did. Needs `TYPESAFE_API_KEY`.
