# The snout binaries.
#
# One derivation for both halves. They are separate crates so that the compiler
# enforces what each may reach for, but they share a lock file and are released
# together — packaging them apart would only invite a machine running a `snout`
# and a `snout-daemon` from different versions.
#
# `cargoLock.lockFile` builds straight from the committed Cargo.lock, so there
# is no vendor hash to regenerate every time a dependency moves — the lock file
# is the single source of truth for what gets built.
{
  lib,
  rustPlatform,
  stdenv,
  version ? "0.1.0",
  rev ? "unknown",
}:
rustPlatform.buildRustPackage {
  pname = "snout";
  inherit version;

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Stamped through the environment rather than a build script that shells out
  # to git: the build sandbox has no repository and no network, so anything
  # derived from git here would silently become "unknown".
  env = {
    SNOUT_VERSION = version;
    SNOUT_COMMIT = rev;
  };

  # Generated from the command table rather than hand-written, so they cannot
  # drift from the real subcommand list. Skipped when cross-compiling, where the
  # freshly built binary cannot be executed.
  postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    install -Dm644 <("$out/bin/snout" completion zsh) \
      "$out/share/zsh/site-functions/_snout"
    install -Dm644 <("$out/bin/snout" completion bash) \
      "$out/share/bash-completion/completions/snout"
  '';

  meta = {
    description = "Simple NixOS Update Tool";
    homepage = "https://github.com/frdrkand/snout";
    license = lib.licenses.mit;
    mainProgram = "snout";
    platforms = lib.platforms.linux;
  };
}
