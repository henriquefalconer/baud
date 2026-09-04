# Reference machine definition for a KVM-capable Intel host.
# Import this from a host configuration and set `hardware.cpu.intel.updateMicrocode`.
{ pkgs, ... }:
{
  imports = [ ../nixos-modules/baud-host.nix ];

  services.baud-host = {
    enable = true;
    user = "baud";
    housekeepingCores = [ 0 1 ];
    isolatedCores = [ 2 3 ];
    storePath = "/var/lib/baud";
  };

  boot.kernelPackages = pkgs.linuxPackages_latest;
  hardware.cpu.intel.updateMicrocode = true;
  virtualisation.libvirtd.enable = false;
}
