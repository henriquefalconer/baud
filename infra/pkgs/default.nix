# infra/pkgs/default.nix — baud fenix cross-compilation overlay
#
# Provides a Nix overlay that adds the following outputs to pkgs:
#   baud-agent        — static musl x86_64-linux baud-tape-agent binary (≤10 MiB)
#   baud-multiverse   — static musl x86_64-linux supervisor binary
#   baud-guests       — example guest binaries (parser, raftlet) as static musl x86_64-linux
#   baud-sandbox-image — OCI container image (agent + supervisor + warm /nix/store)
#
# The overlay depends on:
#   fenix         — Rust toolchain pinning (https://github.com/nix-community/fenix)
#   nixpkgs       — base packages (cargo-zigbuild replaced by fenix musl target)
#
# Usage from flake.nix:
#   overlays = [ (import ./infra/pkgs) ];
#
# Build targets:
#   nix build .#baud-agent         -- cross-compile agent for x86_64-linux-musl
#   nix build .#baud-sandbox-image -- produce OCI image for Daytona
#
# Plan §11.2: "fenix-based cross-compilation overlay that de-risks two items:
#   (1) the snapshot image and (2) the macOS→linux cross-build"

{ pkgs, fenix, ... }:

let
  # Target triple for Daytona sandbox images (x86_64-unknown-linux-musl)
  target = "x86_64-unknown-linux-musl";

  # Fetch the cross-compilation toolchain via fenix
  toolchain = fenix.packages.${pkgs.system}.combine [
    fenix.packages.${pkgs.system}.stable.rustc
    fenix.packages.${pkgs.system}.stable.cargo
    fenix.packages.${pkgs.system}.targets.${target}.stable.rust-std
  ];

  # Cross-compile environment: macOS host → x86_64-unknown-linux-musl
  crossPkgs = pkgs.pkgsCross.musl64;

in {
  # baud-tape-agent: the binary that runs inside Daytona sandboxes.
  # Budget: ≤10 MiB (enforced by a CI check on this derivation's output).
  baud-agent = pkgs.callPackage ./baud-agent.nix {
    inherit toolchain target;
    crateDir = ../..;
  };

  # baud-multiverse: the deterministic supervisor (Linux only).
  baud-multiverse = pkgs.callPackage ./baud-multiverse.nix {
    inherit toolchain target;
    crateDir = ../..;
  };

  # Example guest binaries (parser, raftlet) built as static musl x86_64-linux.
  baud-guests = pkgs.callPackage ./baud-guests.nix {
    inherit toolchain target;
    crateDir = ../..;
  };

  # baud-sandbox-image: OCI image suitable for Daytona snapshot.
  # Contains: agent + supervisor + tracing probes + warm /nix/store.
  # This is the "prebaked snapshot with warm /nix/store" from plan §9.
  baud-sandbox-image = pkgs.callPackage ./baud-sandbox-image.nix {
    inherit (pkgs) dockerTools;
    baud-agent = pkgs.baud-agent or (pkgs.callPackage ./baud-agent.nix { inherit toolchain target; crateDir = ../..; });
    baud-multiverse = pkgs.baud-multiverse or (pkgs.callPackage ./baud-multiverse.nix { inherit toolchain target; crateDir = ../..; });
  };
}
