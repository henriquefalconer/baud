# infra/pkgs/baud-multiverse.nix — cross-compile baud-multiverse supervisor for x86_64-linux-musl
#
# Produces a static musl x86_64-linux binary for the deterministic supervisor.
# The supervisor is Linux-only (uses ptrace + seccomp-unotify).
#
# Plan §11.2: "baud-multiverse.nix — static musl x86_64 supervisor"

{ stdenv, lib, rustPlatform, toolchain, target, crateDir }:

rustPlatform.buildRustPackage rec {
  pname = "baud-multiverse";
  version = "0.1.0";

  src = crateDir;

  cargoBuildFlags = [
    "--package" "baud-multiverse"
    "--target" target
  ];

  CARGO_BUILD_TARGET = target;
  RUSTFLAGS = "-C target-feature=+crt-static";

  meta = with lib; {
    description = "baud deterministic supervisor — ptrace + seccomp-unotify guest interceptor";
    license = licenses.proprietary;
    platforms = [ "x86_64-linux" ];
  };

  cargoLock.lockFile = "${crateDir}/Cargo.lock";
}
