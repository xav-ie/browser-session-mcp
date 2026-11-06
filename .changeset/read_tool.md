---
default: minor
---

# read: only the parts of a page that answer a question

The new `read` tool returns just the passages of the active tab that help
answer a question, under their headings, instead of the whole page. Jev
rates every passage in parallel requests, so a 118,000-character Wikipedia
article comes back as about 12,000 characters in under half a second.
Navigation, sidebars, headers and footers are left out, and a page under
8,000 characters comes back whole without asking Jev. Needs
`TYPESAFE_API_KEY`.
