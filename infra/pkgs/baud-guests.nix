# infra/pkgs/baud-guests.nix — cross-compile example guest binaries for x86_64-linux-musl
#
# Produces static musl x86_64-linux binaries for the example workloads
# (parser, raftlet) that run inside Daytona sandboxes under the supervisor.
# These are for integration testing only; the real guest images come from the
# product teams.
#
# Plan §11.2: "baud-guests.nix — example guest binaries (static musl x86_64-linux)"

{ stdenv, lib, rustPlatform, toolchain, target, crateDir }:

rustPlatform.buildRustPackage rec {
  pname = "baud-guests";
  version = "0.1.0";

  src = crateDir;

  # Build all example guest binaries.
  cargoBuildFlags = [
    "--package" "baud-guests"
    "--target" target
  ];

  CARGO_BUILD_TARGET = target;
  RUSTFLAGS = "-C target-feature=+crt-static";

  # Verify guests are actually static ELF (no dynamic linker dependency).
  postInstall = ''
    for BIN in "$out/bin/"*; do
      if file "$BIN" 2>/dev/null | grep -q "dynamically linked"; then
        echo "ERROR: $BIN is dynamically linked — must be static musl"
        exit 1
      fi
    done
    echo "baud-guests: all binaries are statically linked"
  '';

  meta = with lib; {
    description = "baud example guest binaries (parser, raftlet) for integration testing";
    license = licenses.proprietary;
    platforms = [ "x86_64-linux" ];
  };

  cargoLock.lockFile = "${crateDir}/Cargo.lock";
}
