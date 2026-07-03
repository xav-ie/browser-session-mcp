# Portable (non-NixOS) systemd units for the release tarball, GENERATED from
# nixos-module.nix so they can never drift from what Nix users actually run.
#
# We evaluate the module against a stub package, render each unit, and sanitize
# it into something a non-Nix host can use:
#   * drop the Nix-store Environment= noise the NixOS unit generator injects
#     (PATH / LOCALE_ARCHIVE / TZDIR — all point into /nix/store),
#   * rewrite ExecStart from the store path to the tarball's install path
#     (/usr/local/bin/browser-session), and
#   * layer back the two things the module does *outside* the unit text:
#       - StateDirectory=  (the module creates /var/lib/... via tmpfiles), and
#       - EnvironmentFile=- pointing at /etc/browser-session/browser-session.env
#         so operators can override the baked-in defaults without editing units.
#
# Scope matches the module on purpose: the `mcp` subcommand (a stdio child of
# your MCP client) and Chrome (you run your own) are NOT units here.
{
  lib,
  runCommand,
  writeText,
  hello,
  nixpkgs,
  system,
}:
let
  # A throwaway NixOS eval that imports the real module. `hello` is a stub whose
  # only use is a /bin/browser-session path in ExecStart, which we rewrite away —
  # so it is never built, only its store path is read at eval time.
  eval = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      ./nixos-module.nix
      (_: {
        boot.isContainer = true;
        system.stateVersion = "24.05";

        services.browser-session = {
          enable = true;
          package = hello;
          # Portable installs point the daemons at a Chrome they run themselves
          # (see the README), so don't emit the module's chrome-headless unit or
          # its After=/Requires= ordering onto it.
          chrome.enable = false;
          browserUrl = "http://127.0.0.1:9222";
          # Match packaging/systemd/browser-session.env so the baked-in defaults
          # and the shipped env file agree.
          stateDir = "/var/lib/browser-session";
          # The module asserts this is non-empty when takeover is enabled; the
          # real value is operator-specific and comes from the env file.
          takeover.chromeWsBase = "wss://chrome.example.com";
        };
      })
    ];
  };

  units = eval.config.systemd.units;

  # The three host-side daemons plus the reaper's timer. (No chrome-headless: see
  # chrome.enable above.)
  names = [
    "browser-session-listener.service"
    "browser-session-reaper.service"
    "browser-session-reaper.timer"
    "browser-session-takeover.service"
  ];

  header = ''
    # GENERATED from nixos-module.nix — DO NOT EDIT.
    # Regenerate with `nix build .#systemd-units`.
    # Portable unit for non-Nix installs; NixOS users use the flake's
    # nixosModules.default instead. See README.md in this directory.
  '';

  # Sanitize one rendered unit. `.service` units get StateDirectory= (the module
  # creates state dirs via tmpfiles, which don't ride along in the unit text) and
  # an optional EnvironmentFile override, both appended after ExecStart so the
  # env file wins over the baked-in Environment= defaults; the `.timer` has no
  # ExecStart and is left untouched by those appends.
  gen = name: ''
    {
      printf '%s' ${lib.escapeShellArg header}
      sed -E \
        -e '/^Environment=.*\/nix\/store/d' \
        -e 's|^ExecStart=[^ ]*/bin/browser-session |ExecStart=/usr/local/bin/browser-session |' \
        -e '/^ExecStart=/a EnvironmentFile=-/etc/browser-session/browser-session.env' \
        -e '/^ExecStart=/a StateDirectory=browser-session' \
        ${writeText "${name}.raw" units.${name}.text}
    } > "$out/${name}"
  '';
in
runCommand "browser-session-systemd-units" { } (
  ''
    mkdir -p "$out"
  ''
  + lib.concatMapStrings gen names
)
