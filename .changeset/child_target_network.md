---
default: minor
---

# Network logs include cross-site iframes and workers

`list_network_requests` now shows requests made by a tab's cross-site iframes
and its workers, logged under the tab's current visit. These run as separate
DevTools targets and were missing before, which hid sandboxed third-party code
such as Shopify's checkout web pixels. The iframe's own document request is
still not logged, since Chrome sends it before the iframe can be attached.
