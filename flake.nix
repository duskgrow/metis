{
  description = "Metis development environment (Rust and Node toolchain versions live in their own pin files)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs"; # avoid a second nixpkgs copy in flake.lock
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        node = pkgs.callPackage ./nix/node.nix { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          # Responsibility boundary: flake provides toolchains and ecosystem
          # tools; language dependencies belong to Cargo.lock / pnpm-lock.yaml.
          # No 2nix tooling (crane/naersk/...) in the dev shell.
          packages = [
            rustToolchain # cargo/rustc/rustfmt/clippy/rust-analyzer — version: rust-toolchain.toml
          ] ++ (with pkgs; [
            just # task-layer entry point
            node # dsh runtime — official binary, version: .node-version
            pnpm # launcher only; the actual version comes from package.json packageManager
            dprint # Markdown/TOML/TypeScript formatter (config: dprint.json)
            prek # git hook runner (reads .pre-commit-config.yaml)
            cargo-nextest # test runner (process isolation / sharding)
            cargo-insta # snapshot review
            cargo-deny # dependency policy: advisories/licenses/bans/sources
            cargo-semver-checks # public-API breakage gate (pre-release)
            cargo-dist # cross-platform release; keep in sync with dist-workspace.toml
            actionlint # static analysis for GitHub Actions workflows
            nixd # Nix language server
            nil # Nix language server (both are provided; an editor picks one)
          ]) ++ (with pkgs; lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            clang
            cmake
            fontconfig
            libxkbcommon
            libxcb
            libX11
            openssl
            pkg-config
            vulkan-loader
            wayland
            webkitgtk_4_1
            zstd
          ]);

          # GPUI dlopens the Wayland client libraries at runtime; nothing links
          # them, so Nix's automatic RUNPATH does not cover them. Vulkan and EGL
          # are loaded the same way by the GPU backend.
          LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux
            (pkgs.lib.makeLibraryPath [
              pkgs.vulkan-loader
              pkgs.libglvnd
              pkgs.wayland
            ]);

          shellHook = ''
            # Git hooks cannot travel with a clone (git's security model), so the
            # devShell arms them on entry — idempotent and skipped in CI and
            # before `just init` creates .git. `--force` because prek owns every
            # hook here: it drops stale `<hook>.legacy` copies that would keep the
            # shim in migration mode. Non-Nix shells: `just setup`.
            if [ -e .git ] && [ -z "''${CI:-}" ]; then
              if ! prek install --force --hook-type pre-commit --hook-type commit-msg >/dev/null 2>&1; then
                echo "warning: could not install git hooks; run 'just setup' to diagnose" >&2
              fi
            fi
            # Banner goes to stderr: stdout of `nix develop -c <cmd>` must stay
            # clean for output-capturing callers (rust-cache's cmd-format parses it).

            echo "devShell ready. Entry point: just --list" >&2
          '';
        };

      })
    ;
}
