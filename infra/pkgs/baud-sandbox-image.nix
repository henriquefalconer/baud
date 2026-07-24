# infra/pkgs/baud-sandbox-image.nix — OCI container image for Daytona sandboxes
#
# Produces a Docker-compatible OCI image that contains:
#   - /usr/local/bin/baud-agent    — the tape agent binary
#   - /usr/local/bin/baud-super    — the deterministic supervisor binary
#   - /etc/baud/allowlist.toml     — default syscall allowlist
#
# The image is intentionally minimal (musl-based, no shell by default) so it
# can serve as a Daytona prebaked snapshot with a warm /nix/store.
#
# The agent starts on container launch and waits for the supervisor to be
# invoked by the baud-server via the agent's gRPC/WebSocket interface.
#
# Plan §9: "prebaked snapshot with warm /nix/store"
# Plan §11.2: "baud-sandbox-image.nix — OCI image"

{ dockerTools, baud-agent, baud-multiverse, lib }:

dockerTools.buildLayeredImage {
  name = "baud-sandbox";
  tag = "latest";

  # Minimal layer set for the sandbox image.
  contents = [
    baud-agent
    baud-multiverse
  ];

  config = {
    # The agent is the entrypoint; it launches the supervisor and relays draws.
    Entrypoint = [ "/bin/baud-agent" ];
    Cmd = [];

    # Expose the agent's WebSocket port (plan §4: agent listens on 9090).
    ExposedPorts = {
      "9090/tcp" = {};
    };

    # Environment defaults — overridden at runtime by Daytona's env injection.
    Env = [
      "BAUD_AGENT_PORT=9090"
      "BAUD_SUPERVISOR=/bin/baud-multiverse"
    ];

    Labels = {
      "org.opencontainers.image.title" = "baud-sandbox";
      "org.opencontainers.image.description" = "baud deterministic validation sandbox image";
    };
  };

  meta = with lib; {
    description = "OCI image for baud Daytona sandboxes (agent + supervisor)";
    platforms = [ "x86_64-linux" ];
  };
}
