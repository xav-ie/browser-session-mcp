---
default: patch
---

# Dependencies refreshed

A full `cargo update` — rustls 0.23.46, quinn-proto 0.11.19, getrandom 0.4 among them — clears both Rust security alerts, and pnpm overrides (`fast-uri` 3.1.9, `brace-expansion` 5.0.12) clear all twelve npm alerts. Also folds in Dependabot's schemars/syn and getrandom bumps.
