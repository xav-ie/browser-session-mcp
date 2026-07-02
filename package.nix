{
  lib,
  rustPlatform,
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

  # Static Astro takeover UI, served by the daemon out of $TAKEOVER_WEBROOT.
  frontend = stdenvNoCC.mkDerivation (finalAttrs: {
    pname = "browser-session-mcp-frontend";
    inherit version;
    src = ./frontend;
    nativeBuildInputs = [
      nodejs_22
      pnpm_10
      pnpm10ConfigHook
    ];
    pnpmDeps = fetchPnpm10Deps {
      inherit (finalAttrs) pname version src;
      fetcherVersion = 3;
      hash = "sha256-kMv0/b4cb1F1LhuPzp6QIYFcUqyqdS+fr2y6wW1hf3Y=";
    };
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
in
rustPlatform.buildRustPackage {
  pname = "browser-session-mcp";
  inherit version;

  src = lib.cleanSourceWith {
    src = ./.;
    filter =
      path: _type:
      let
        base = baseNameOf path;
      in
      !(
        base == "package.nix"
        || base == "flake.nix"
        || base == "flake.lock"
        || base == "frontend" # built separately as `frontend`; not part of the Rust build
        || base == "target"
        || base == "result"
        || base == ".direnv"
        || lib.hasSuffix ".log" base
      );
  };

  cargoLock = {
    lockFile = ./Cargo.lock;
    # chromiumoxide is our git fork (see [patch.crates-io] in Cargo.toml). It is a
    # workspace, so the fork supplies all four member crates via git; they share
    # one checkout and therefore one hash. Recompute after bumping the fork rev:
    #   nix run nixpkgs#nix-prefetch-git -- \
    #     --url https://github.com/xav-ie/chromiumoxide.git --rev <rev>
    outputHashes =
      let
        forkHash = "sha256-rSZ5ruvRZK+C7vZxT2r1M8ywW4zUcTfPkp48P0RoidE=";
      in
      {
        "chromiumoxide-0.9.1" = forkHash;
        "chromiumoxide_cdp-0.9.1" = forkHash;
        "chromiumoxide_pdl-0.9.1" = forkHash;
        "chromiumoxide_types-0.9.1" = forkHash;
      };
  };

  # aws-lc-rs (pulled by reqwest's rustls-tls feature) needs cmake and a C
  # toolchain at build time; makeWrapper to point the daemon at the built UI.
  nativeBuildInputs = [
    cmake
    pkg-config
    makeWrapper
  ];

  # aws-lc-rs's build script invokes cmake which expects to manage its own
  # build dir; nixpkgs' default cmake hook gets in the way.
  dontUseCmakeConfigure = true;

  doCheck = false;

  # `browser-session takeover` serves the built Astro UI from TAKEOVER_WEBROOT.
  # Wrapping the one multi-call binary is harmless for the other subcommands.
  postInstall = ''
    wrapProgram $out/bin/browser-session \
      --set TAKEOVER_WEBROOT ${frontend}
  '';

  meta = {
    description = "MCP server giving each caller an isolated browser session against a shared persistent Chrome, with a human-takeover web UI.";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "browser-session";
  };
}
