{
  description = "browser-session-mcp — per-caller isolated browser sessions over MCP against a shared persistent Chrome, with a human-takeover web UI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { moduleWithSystem, ... }:
      {
        systems = [
          "x86_64-linux"
          "aarch64-linux"
        ];
        imports = [ inputs.treefmt-nix.flakeModule ];

        # NixOS module for the host-side stack (persistent Chrome + the listener /
        # reaper / takeover daemons), with `package` defaulted to this flake's
        # build for the consuming host's system.
        flake.nixosModules.browser-session = moduleWithSystem (
          perSystem@{ config }:
          { lib, ... }:
          {
            imports = [ ./nixos-module.nix ];
            services.browser-session.package = lib.mkDefault config.packages.browser-session-mcp;
          }
        );
        flake.nixosModules.default = inputs.self.nixosModules.browser-session;

        perSystem =
          { config, pkgs, ... }:
          {
            packages.browser-session-mcp = pkgs.callPackage ./package.nix { };
            packages.default = config.packages.browser-session-mcp;

            # The package (all four binaries) must build.
            checks.build = config.packages.default;

            # End-to-end integration test in a throwaway NixOS VM. It imports the
            # real nixos-module.nix, enables the whole stack (persistent Chrome +
            # listener + reaper timer + takeover daemon), and asserts the systemd
            # wiring actually comes up — then runs the same scripts/smoke.sh that
            # developers run locally to exercise the full MCP tool surface
            # (open/navigate/snapshot/evaluate/save-state/tabs/close) against a
            # real Chrome over CDP. So one check covers both the module and the
            # protocol. `nix flake check` runs it automatically.
            #
            # Note: NixOS VM tests are x86_64/aarch64-linux only and need KVM on
            # the builder (see the `nixos-test` CI job).
            checks.smoke = pkgs.testers.runNixOSTest {
              name = "browser-session-smoke";
              nodes.machine =
                { pkgs, ... }:
                {
                  imports = [ ./nixos-module.nix ];
                  # Two Chromes are live during the run (the module's persistent
                  # one + the self-contained one smoke.sh spins up), so give the
                  # VM headroom.
                  virtualisation.memorySize = 4096;

                  services.browser-session = {
                    enable = true;
                    package = config.packages.browser-session-mcp;
                    chrome.package = pkgs.ungoogled-chromium;
                    chrome.executable = "chromium";
                    # The module targets chrome-headless-shell (headless by
                    # default); plain chromium must be told to run headless AND
                    # to use the headless ozone backend (old `--headless` is a
                    # no-op in current chromium, so it still tries X11), and the
                    # VM has no GPU.
                    chrome.extraArgs = [
                      "--headless=new"
                      "--ozone-platform=headless"
                      "--disable-gpu"
                    ];
                    # takeover asserts a non-empty chromeWsBase; the daemon only
                    # binds + serves here (nothing connects), so a dummy suffices.
                    takeover.chromeWsBase = "ws://127.0.0.1:9222";
                  };

                  # smoke.sh's harness: coproc/bash, JSON extraction, /json probe.
                  environment.systemPackages = [
                    config.packages.browser-session-mcp
                    pkgs.bash
                    pkgs.python3
                    pkgs.curl
                  ];
                };
              testScript = ''
                machine.wait_for_unit("multi-user.target")

                # --- module wiring: the host-side daemons come up ---
                machine.wait_for_unit("chrome-headless.service")
                machine.wait_for_open_port(9222)
                machine.wait_for_unit("browser-session-listener.service")
                machine.wait_for_unit("browser-session-takeover.service")
                machine.wait_for_open_port(9223)
                machine.succeed("curl -fsS http://127.0.0.1:9223/healthz")

                # The reaper is a timer-driven oneshot; trigger a sweep now and
                # assert it exits cleanly against the live Chrome.
                machine.succeed("systemctl start browser-session-reaper.service")
                machine.succeed("systemctl is-active --quiet chrome-headless.service")

                # --- protocol: the full MCP tool surface end-to-end ---
                machine.succeed(
                    "env "
                    "CHROME_BIN=${pkgs.ungoogled-chromium}/bin/chromium "
                    "MCP_BIN=${config.packages.browser-session-mcp}/bin/browser-session "
                    "bash ${./scripts/smoke.sh} 2>&1"
                )
              '';
            };

            # treefmt covers Nix + Rust; the frontend keeps its own pnpm
            # prettier/eslint/astro-check toolchain (run in CI), so it's excluded
            # here. Importing the flakeModule also adds a `treefmt` flake check.
            treefmt = {
              projectRootFile = "flake.nix";
              programs.nixfmt.enable = true;
              programs.rustfmt.enable = true;
              settings = {
                on-unmatched = "info";
                excludes = [
                  "Cargo.lock"
                  "flake.lock"
                  "*.lock"
                  "result"
                  ".direnv/**"
                  "target/**"
                  "frontend/**"
                ];
              };
            };

            devShells.default = pkgs.mkShell {
              packages = [
                pkgs.cargo
                pkgs.rustc
                pkgs.rustfmt
                pkgs.clippy
                pkgs.rust-analyzer
                pkgs.cmake
                pkgs.pkg-config
                # Frontend toolchain (matches the pinned pnpm).
                pkgs.nodejs_22
                pkgs.pnpm_10
                config.treefmt.build.wrapper
              ];
            };
          };
      }
    );
}
