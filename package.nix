{
  lib,
  craneLib,
  cmake,
  pkg-config,
  makeWrapper,
  stdenvNoCC,
  nodejs_22,
  pnpm_10,
  pnpmConfigHook,
  fetchPnpmDeps,
}:
let
  version = "0.1.0";

  # Pin the pnpm hooks to pnpm 10 so the build agrees with the committed lockfile.
  pnpm10ConfigHook = pnpmConfigHook.override { pnpm = pnpm_10; };
  fetchPnpm10Deps = fetchPnpmDeps.override { pnpm = pnpm_10; };

  # pnpm's fd juggling makes Node emit tens of thousands of harmless "File
  # descriptor opened in unmanaged mode" *process* warnings on Darwin (Linux is
  # silent). --no-warnings mutes them at the source: pnpm install (the pnpmDeps
  # fixed-output derivation — hash is content-addressed, so it's unchanged) and
  # pnpm build below.
  pnpmQuiet = "--no-warnings";

  # Static Astro takeover UI, served by the daemon out of $TAKEOVER_WEBROOT.
  frontend = stdenvNoCC.mkDerivation (finalAttrs: {
    pname = "browser-session-mcp-frontend";
    inherit version;
    src = ./frontend;
    NODE_OPTIONS = pnpmQuiet;
    nativeBuildInputs = [
      nodejs_22
      pnpm_10
      pnpm10ConfigHook
    ];
    pnpmDeps =
      (fetchPnpm10Deps {
        inherit (finalAttrs) pname version src;
        fetcherVersion = 3;
        hash = "sha256-kMv0/b4cb1F1LhuPzp6QIYFcUqyqdS+fr2y6wW1hf3Y=";
      }).overrideAttrs
        (o: {
          env = (o.env or { }) // {
            NODE_OPTIONS = pnpmQuiet;
          };
        });
    buildPhase = ''
      runHook preBuild
      pnpm build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  });
  # Only the Rust sources (Cargo.toml/lock + *.rs); drops the frontend, nix
  # files, target/, etc. so a doc/frontend edit doesn't invalidate the build.
  src = craneLib.cleanCargoSource ./.;

  commonArgs = {
    inherit src version;
    pname = "browser-session-mcp";
    strictDeps = true;

    # aws-lc-rs (via reqwest's rustls feature) needs cmake + a C toolchain to
    # build. Its build script runs its own cmake, so suppress the nixpkgs cmake
    # configure hook (there's no CMakeLists at the crate root).
    nativeBuildInputs = [
      cmake
      pkg-config
    ];
    dontUseCmakeConfigure = true;

    # No unit tests in the crate; skip crane's default `cargo test` phase.
    doCheck = false;
  };

  # The dependency closure (aws-lc-rs, the chromiumoxide git fork, rustls, …)
  # built on its own. Keyed on Cargo.lock + toolchain, independent of src/, so
  # source-only changes reuse it — this is the layer the Nix store / magic-nix-
  # cache reuses across builds. Git deps (the fork) are fetched from Cargo.lock;
  # no manual outputHashes needed.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ makeWrapper ];

    # `browser-session takeover` serves the built Astro UI from TAKEOVER_WEBROOT.
    # Wrapping the one multi-call binary is harmless for the other subcommands.
    postInstall = ''
      wrapProgram $out/bin/browser-session \
        --set TAKEOVER_WEBROOT ${frontend}
    '';

    meta = {
      description = "MCP server giving each caller an isolated browser session against a shared persistent Chrome, with a human-takeover web UI.";
      license = lib.licenses.mit;
      platforms = lib.platforms.unix;
      mainProgram = "browser-session";
    };
  }
)
