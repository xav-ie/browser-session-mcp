{
  description = "browser-session-mcp — per-caller isolated browser sessions over MCP against a shared persistent Chrome, with a human-takeover web UI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    # Provides a rust toolchain with the musl target added, whose self-contained
    # musl honours `+crt-static` — the only reliable way to get a fully static
    # (portable, non-Nix) release binary. Used only by `packages.release`.
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { moduleWithSystem, ... }:
      {
        systems = [
          "x86_64-linux"
          "aarch64-linux"
          "aarch64-darwin"
          # Intel macOS: only to expose packages.x86_64-darwin.release for the
          # release tarball (CI's Nix checks don't target it).
          "x86_64-darwin"
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
          {
            config,
            lib,
            pkgs,
            system,
            ...
          }:
          {
            packages = {
              browser-session-mcp = pkgs.callPackage ./package.nix {
                craneLib = inputs.crane.mkLib pkgs;
              };
              default = config.packages.browser-session-mcp;

              # Unwrapped binary for the release tarball. On Darwin this is an
              # ordinary Nix build — macOS cannot be statically linked (libSystem
              # is always dynamic), so the darwin tarball's binary needs /nix/store
              # present, i.e. Nix installed. On Linux this attr is replaced below
              # by a fully static musl build that runs on any host.
              release = pkgs.callPackage ./package.nix {
                craneLib = inputs.crane.mkLib pkgs;
                wrapUi = false;
              };
            }
            # Portable systemd units for the release tarball, generated from
            # nixos-module.nix so they can't drift from what NixOS users run.
            # Linux-only: it evaluates a NixOS system (no darwin equivalent).
            // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              systemd-units = pkgs.callPackage ./systemd-units.nix {
                nixpkgs = inputs.nixpkgs;
                system = pkgs.stdenv.hostPlatform.system;
              };

              # Fully static (musl) release binary: no dynamic loader, so nothing
              # points into /nix/store and it runs on any Linux host, Nix or not.
              #
              # Recipe (crane's documented one): a rust-overlay toolchain with the
              # musl target added — its self-contained musl honours `+crt-static`,
              # unlike nixpkgs' cross-musl (dynamic) or pkgsStatic (forces the
              # host-side build scripts static → they fail to link glibc). aws-lc's
              # C is compiled for the target by the musl cc borrowed from the
              # nixpkgs cross set (proven to build aws-lc under musl).
              release =
                let
                  isArm = pkgs.stdenv.hostPlatform.isAarch64;
                  muslTarget = if isArm then "aarch64-unknown-linux-musl" else "x86_64-unknown-linux-musl";
                  muslCc =
                    (if isArm then pkgs.pkgsCross.aarch64-multiplatform-musl else pkgs.pkgsCross.musl64).stdenv.cc;
                  rustPkgs = import inputs.nixpkgs {
                    inherit system;
                    overlays = [ inputs.rust-overlay.overlays.default ];
                  };
                  toolchain = rustPkgs.rust-bin.stable.latest.default.override {
                    targets = [ muslTarget ];
                  };
                  craneLibMusl = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;
                in
                pkgs.callPackage ./package.nix {
                  craneLib = craneLibMusl;
                  wrapUi = false;
                  staticTarget = muslTarget;
                  staticCc = muslCc;
                };
            };

            checks = {
              # The package (all four binaries) must build.
              build = config.packages.default;
            }
            # End-to-end integration test in a throwaway NixOS VM. It imports the
            # real nixos-module.nix, enables the whole stack (persistent Chrome +
            # listener + reaper timer + takeover daemon), and asserts the systemd
            # wiring actually comes up — then runs the same scripts/smoke.sh that
            # developers run locally to exercise the full MCP tool surface
            # (open/navigate/snapshot/evaluate/save-state/tabs/close) against a
            # real Chrome over CDP. So one check covers both the module and the
            # protocol. `nix flake check` runs it automatically.
            #
            # Note: NixOS VM tests are Linux-only and need KVM on the builder
            # (see the `nixos-test` CI job), so this check is omitted on Darwin —
            # the flake still targets aarch64-darwin for the package build.
            // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              smoke = pkgs.testers.runNixOSTest {
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
