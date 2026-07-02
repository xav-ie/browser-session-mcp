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
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      imports = [ inputs.treefmt-nix.flakeModule ];

      perSystem =
        { config, pkgs, ... }:
        {
          packages.browser-session-mcp = pkgs.callPackage ./package.nix { };
          packages.default = config.packages.browser-session-mcp;

          # The package (all four binaries) must build.
          checks.build = config.packages.default;

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
    };
}
