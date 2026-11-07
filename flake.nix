{
  description = "SNOUT — Simple NixOS Update Tool";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
  };

  # Two inputs, and the second is only a module system. The dev shell is a plain
  # mkShell rather than devenv.sh: devenv pulled eleven transitive inputs for a
  # shell that needs a Rust toolchain and nixfmt, and it derives its own root
  # from $PWD, which is empty under pure evaluation — so `nix flake check`
  # tripped devenv's assertion before it checked anything. A tool this small
  # should not need an --impure escape hatch to check itself.

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      perSystem =
        {
          pkgs,
          lib,
          self',
          system,
          ...
        }:
        let
          # The workspace and nothing else. Without this the fmt and clippy
          # checks take the whole flake as input, so editing the README re-runs
          # clippy.
          rustSource = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };

          # Everything the checks need to compile the crate offline, in the same
          # shape `buildRustPackage` sets up: the lock file's crates vendored
          # into a directory, and a .cargo/config.toml pointing at it.
          cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };

          # A check that compiles has to be able to link, so these are
          # mkDerivation-based rather than runCommand: runCommand is stdenvNoCC
          # and has no compiler driver for rustc to invoke.
          cargoCheck =
            name: script:
            pkgs.stdenv.mkDerivation {
              inherit name;
              src = rustSource;
              nativeBuildInputs = with pkgs; [
                cargo
                rustc
                clippy
                rustfmt
              ];
              buildPhase = ''
                export HOME="$TMPDIR"
                export CARGO_HOME="$TMPDIR/cargo-home"
                mkdir -p "$CARGO_HOME"
                ln -s ${cargoDeps} "$TMPDIR/vendor"
                cat > "$CARGO_HOME/config.toml" <<'EOF'
                [source.crates-io]
                replace-with = "vendored-sources"
                [source.vendored-sources]
                directory = "${cargoDeps}"
                EOF
                ${script}
              '';
              installPhase = "touch $out";
            };

          # One task runner, not two: these are on $PATH inside `nix develop`.
          devScripts = {
            snout-test = ''exec cargo test --workspace "$@"'';
            snout-build = ''exec cargo build --workspace --release "$@"'';
            snout-fmt = ''
              set -e
              cargo fmt --all
              nixfmt .
            '';
            # What `nix flake check` runs, minus the nix evaluation — so a
            # failure here is a failure there, without waiting on a build.
            snout-check = ''
              set -e
              cargo fmt --all -- --check
              cargo clippy --workspace --all-targets -- -D warnings
              cargo test --workspace
            '';
          };
        in
        {
          packages.default = pkgs.callPackage ./nix/package.nix {
            rev = inputs.self.shortRev or inputs.self.dirtyShortRev or "unknown";
          };
          packages.snout = self'.packages.default;

          devShells.default = pkgs.mkShell {
            name = "snout";

            packages = [
              # The same toolchain the package is built with — one nixpkgs pin,
              # so the shell cannot drift from the build.
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.nixfmt # the rfc-style one; the -rfc-style alias now warns
              pkgs.jq # not used by snout, but useful when poking at its JSON
            ]
            ++ lib.mapAttrsToList pkgs.writeShellScriptBin devScripts;

            shellHook = ''
              echo "snout dev shell — snout-test | snout-build | snout-fmt | snout-check"
            '';
          };

          checks = {
            package = self'.packages.default;

            # Built for real, not just evaluated: forcing the whole system
            # closure is what makes a typo in the systemd unit fail here rather
            # than on the machine that imports this. Stopping at the generated
            # config.json would leave the unit — ExecStart, path, the mkIf —
            # unevaluated, since nothing else forces it. It is also what
            # catches an ExecStart naming a binary the package does not build.
            module =
              (inputs.nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  inputs.self.nixosModules.default
                  ./nix/check-module.nix
                ];
              }).config.system.build.toplevel;

            fmt = cargoCheck "snout-fmt-check" "cargo fmt --all -- --check";

            clippy = cargoCheck "snout-clippy" ''
              cargo clippy --frozen --offline --workspace --all-targets -- -D warnings
            '';
          };

          formatter = pkgs.nixfmt-tree;
        };

      flake.nixosModules.default = import ./nix/module.nix { inherit (inputs) self; };
    };
}
