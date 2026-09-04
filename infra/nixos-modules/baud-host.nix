# Baud host module.
#
# This module configures the host assumptions checked by `baud host probe`: Intel KVM,
# one physical core per guest, a small housekeeping set, and explicit storage paths.
{ config, lib, pkgs, ... }:

let
  cfg = config.services.baud-host;
  inherit (lib) mkEnableOption mkOption types mkIf;
in
{
  options.services.baud-host = {
    enable = mkEnableOption "the baud deterministic KVM host";

    user = mkOption {
      type = types.str;
      default = "baud";
      description = "Account allowed to open /dev/kvm and run the baud service.";
    };

    housekeepingCores = mkOption {
      type = types.listOf types.int;
      default = [ 0 1 ];
      description = "CPU ids reserved for the host, interrupts, and storage work.";
    };

    isolatedCores = mkOption {
      type = types.listOf types.int;
      default = [ 2 3 ];
      description = "CPU ids reserved for vCPU workers and excluded from general scheduling.";
    };

    storePath = mkOption {
      type = types.path;
      default = "/var/lib/baud";
      description = "Content-addressed snapshot and image store.";
    };

    secretFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Optional sops-rendered secret file consumed by the server.";
    };
  };

  config = mkIf cfg.enable {
    boot.kernelModules = [ "kvm-intel" ];
    boot.kernelParams = [
      "kvm-intel.nested=1"
      "isolcpus=managed_irq,${lib.concatStringsSep "," (map toString cfg.isolatedCores)}"
      "nohz_full=${lib.concatStringsSep "," (map toString cfg.isolatedCores)}"
      "rcu_nocbs=${lib.concatStringsSep "," (map toString cfg.isolatedCores)}"
    ];

    users.groups.kvm = {};
    users.users.${cfg.user}.extraGroups = [ "kvm" ];

    systemd.tmpfiles.rules = [
      "d ${cfg.storePath} 0750 ${cfg.user} kvm - -"
    ];

    environment.etc."baud/host.conf".text = lib.generators.toTOML {} {
      store_path = toString cfg.storePath;
      housekeeping_cores = cfg.housekeepingCores;
      require_intel = true;
      require_kvm = true;
      require_smt_safe_placement = true;
      secret_file = if cfg.secretFile == null then null else toString cfg.secretFile;
    };

    assertions = [
      {
        assertion = builtins.length cfg.housekeepingCores >= 2;
        message = "services.baud-host.housekeepingCores must reserve at least two CPUs";
      }
      {
        assertion = builtins.all (cpu: cpu >= 0) (cfg.housekeepingCores ++ cfg.isolatedCores);
        message = "services.baud-host CPU lists must contain non-negative CPU ids";
      }
      {
        assertion = lib.intersectLists cfg.housekeepingCores cfg.isolatedCores == [];
        message = "services.baud-host housekeepingCores and isolatedCores must not overlap";
      }
    ];

    environment.systemPackages = [ pkgs.util-linux ];
  };
}
