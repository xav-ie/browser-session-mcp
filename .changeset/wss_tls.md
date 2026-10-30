---
default: patch
---

# wss TLS restored

A dependency update had left the crate's direct `async-tungstenite` on 0.35 while `chromiumoxide` still resolved 0.32, and cargo features never unify across versions, so browser-level `wss://` attaches failed with "TLS support not compiled in". The direct dependency is pinned back to 0.32 with a comment tying it to `chromiumoxide`'s line.
