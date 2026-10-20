# Development

_[← Back to README](../README.md) · [Docs index](README.md)_

- [Setup](#setup)
- [Checks](#checks)
- [Diagrams](#diagrams)
- [Releases](#releases)

## Setup

`nix develop` gives you cargo, rustc, clippy, node and pnpm for the takeover UI,
treefmt and knope. Point a dev build at any Chrome with its DevTools port open:

```sh
cargo build
BROWSER_URL=http://localhost:9222 ./target/debug/browser-session mcp
```

The takeover UI lives in [`frontend/`](../frontend); `pnpm dev` there runs it
against a live takeover daemon.

## Checks

Every push and pull request runs these; run them locally before you push:

| Check | Locally | Workflow |
|---|---|---|
| Build, clippy, formatting, NixOS VM test | `nix flake check` (formatting: `nix fmt`) | CI |
| Takeover UI types, lint, formatting | `pnpm check:astro`, `pnpm check:lint`, `pnpm check:format` in `frontend/` | CI |
| Workflows | `actionlint`, `zizmor .github` | Lint |
| Shell scripts | `shellcheck scripts/*.sh` | Lint |
| Diagrams up to date | `node scripts/diagrams.mjs`, then `git status` | Lint |
| Commit messages | summary up to 72 characters, body up to 6 lines | Lint |
| CodeQL (Rust, TypeScript, workflows) | | Security |

A push that only touches docs skips the build jobs.

## Diagrams

The diagrams in these docs are generated SVGs, light and dark, from
[`scripts/diagrams.mjs`](../scripts/diagrams.mjs) (boxes, arrows and labels as
data) and [`docs/diagrams/palette.json`](diagrams/palette.json) (the takeover
UI's purple and yellow). The script also writes each diagram's `<picture>` and
alt text into every `<!-- diagram:NAME -->` block in the docs. Edit those, run
it, and commit the SVGs and docs with them:

```sh
node scripts/diagrams.mjs
```

## Releases

Releases use [knope](https://knope.tech) change files. When a change should show
up in the release notes, run `knope document-change` (in `nix develop`): it asks
for the kind of change (`major` breaks something, `minor` adds something,
`patch` fixes something) and a one-line summary, and writes a note to
`.changeset/`. Commit it with the change.

Every push to main then updates a "chore: prepare release" pull request, which
bumps the version in `Cargo.toml`, `Cargo.lock` and `package.nix` and moves the
notes into [`CHANGELOG.md`](../CHANGELOG.md). Merging it builds a tarball per
platform (static Linux binaries for x86_64 and aarch64, and an Apple silicon
build that needs Nix) and publishes them with a `SHA256SUMS` file as a GitHub
release. To choose the version yourself (such as 1.0.0), run the Prepare release
workflow by hand with a version.
