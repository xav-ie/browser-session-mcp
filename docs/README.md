# Documentation

_[← Back to project README](../README.md)_

Start at the project [README](../README.md) for the pitch and
[Quick start](../README.md#quick-start). These docs go deeper, in the order most
people want them:

1. **[Workflows](workflows.md)** — read when you need a login the agent must not
   see ([human takeover](workflows.md#human-takeover)) or want to reuse cookies
   across sessions ([saved cookie state](workflows.md#saved-cookie-state)).
2. **[How it works](how-it-works.md)** — read to understand the design: why
   [sessions are a tool argument](how-it-works.md#sessions-are-a-tool-argument),
   the [four-process architecture](how-it-works.md#architecture), the
   [anti-detection/stealth](how-it-works.md#anti-detection-stealth) story, and the
   on-disk [storage layout](how-it-works.md#storage-layout).
3. **[Deployment](deployment.md)** — read when you're ready to run it for real:
   [building from source](deployment.md#building-from-source-nix), the
   [NixOS module](deployment.md#nixos-module),
   [systemd without Nix](deployment.md#running-the-daemons-without-nix-systemd),
   the full [env-var reference](deployment.md#environment), and
   [operational notes](deployment.md#operational-notes).
