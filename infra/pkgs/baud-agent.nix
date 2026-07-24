# infra/pkgs/baud-agent.nix — cross-compile baud-tape-agent for x86_64-linux-musl
#
# Produces a static musl x86_64-linux binary ≤10 MiB containing the agent
# that runs inside Daytona sandboxes.  Binary size is enforced by a
# post-install check.
#
# Plan §11.2: "baud-agent.nix — static musl x86_64 baud-tape-agent"

{ stdenv, lib, rustPlatform, toolchain, target, crateDir }:

rustPlatform.buildRustPackage rec {
  pname = "baud-agent";
  version = "0.1.0";

  src = crateDir;

  # Only build the baud-tape-agent binary; skip all other workspace members.
  cargoBuildFlags = [
    "--package" "baud-tape-agent"
    "--bin" "baud-agent"
    "--target" target
  ];

  # Musl cross-compilation requires a C cross-toolchain for linking.
  CARGO_BUILD_TARGET = target;
  RUSTFLAGS = "-C target-feature=+crt-static";

  # Binary size budget: ≤10 MiB (plan §4, baud-tape-agent spec §2)
  postInstall = ''
    SIZE=$(stat -c%s "$out/bin/baud-agent" 2>/dev/null || stat -f%z "$out/bin/baud-agent")
    MAX=$((10 * 1024 * 1024))
    if [ "$SIZE" -gt "$MAX" ]; then
      echo "ERROR: baud-agent binary exceeds 10 MiB budget: $SIZE bytes"
      exit 1
    fi
    echo "baud-agent size OK: $SIZE bytes (budget: $MAX bytes)"
  '';

  meta = with lib; {
    description = "baud tape agent — runs inside Daytona sandboxes to launch the supervisor and stream observations";
    license = licenses.proprietary;
    platforms = [ "x86_64-linux" ];
  };

  # Cargo.lock is committed in the workspace root
  cargoLock.lockFile = "${crateDir}/Cargo.lock";
}
