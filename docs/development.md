# Development

_[← Back to README](../README.md) · [Docs index](README.md)_

- [Setup](#setup)
- [Checks](#checks)
- [Diagrams, logo and screenshots](#diagrams-logo-and-screenshots)
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
| Diagrams and logo up to date | `node scripts/diagrams.mjs` and `node scripts/logo.mjs`, then `git status` | Lint |
| Commit messages | summary up to 72 characters, body up to 6 lines | Lint |
| CodeQL (Rust, TypeScript, workflows) | | Security |

A push that only touches docs skips the build jobs.

Main has a ruleset: changes land through pull requests whose **CI passed** and
**Lint passed** checks succeed. Those two jobs stand for every other job in
their workflow, and count a job skipped by the docs-only filter as passing. It
also blocks force pushes and deleting main. Repository admins can bypass it,
which pushing straight to main and merging the release PR both need: the
release PR is opened with the workflow's own token, and GitHub doesn't run
workflows for pull requests opened that way.

## Diagrams, logo and screenshots

The diagrams in these docs are generated SVGs, light and dark, from
[`scripts/diagrams.mjs`](../scripts/diagrams.mjs) (boxes, arrows and labels as
data) and [`docs/diagrams/palette.json`](diagrams/palette.json) (the takeover
UI's purple and yellow). The script also writes each diagram's `<picture>` and
alt text into every `<!-- diagram:NAME -->` block in the docs. Edit those, run
it, and commit the SVGs and docs with them:

```sh
node scripts/diagrams.mjs
```

The logo, a turtle whose shell is a browser tab, comes from
[`scripts/logo.mjs`](../scripts/logo.mjs) in the same palette. It writes
[`docs/logo/logo.svg`](logo/logo.svg), the takeover page's favicon and the repo's social preview,
`docs/logo/social-preview.svg`. GitHub takes the social preview as a PNG, uploaded
under Settings → General → Social preview; after changing the logo, render the
PNG again and upload it:

```sh
node scripts/logo.mjs
nix run nixpkgs#librsvg -- -o docs/logo/social-preview.png docs/logo/social-preview.svg
```

The README's screenshot of the takeover page is real:
[`scripts/screenshot.mjs`](../scripts/screenshot.mjs) starts a Chrome for the
agent's session, the takeover daemon and the MCP server, opens a session on
GitHub's sign-in page, and screenshots the takeover link from a second Chrome.
Everything runs on ports the OS picks, so it never touches a stack you already
have running. After changing the takeover UI, rebuild and retake it:

```sh
nix build
node scripts/screenshot.mjs              # or pass another URL to show
```

It needs Chrome (`CHROME` picks the binary, `google-chrome` by default) and the
network, so CI doesn't run it.

## Releases

Releases use [knope](https://knope.tech) change files. When a change should show
up in the release notes, run `knope document-change` (in `nix develop`): it asks
for the kind of change (`major` breaks something, `minor` adds something,
`patch` fixes something) and a one-line summary, and writes a note to
`.changeset/`. Commit it with the change.

Every push to main then updates a "chore: prepare release" pull request, which
bumps the version in `Cargo.toml`, `Cargo.lock` and `package.nix` and moves the
notes into [`CHANGELOG.md`](../CHANGELOG.md). Merging it builds a tarball per
platform (static Linux binaries for x86_64 and aarch64, and one for Apple
silicon) and publishes them with a `SHA256SUMS` file as a GitHub
release. To choose the version yourself (such as 1.0.0), run the Prepare release
workflow by hand with a version.
