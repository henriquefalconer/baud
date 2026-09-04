# Baud machine definitions

`baud-x86_64.nix` is the reference composition for an Intel KVM host. It keeps two CPUs for the
host and reserves separate CPUs for vCPU workers, matching `baud-host`'s rule that SMT siblings
are never split between guests. The module writes the store and CPU policy into `/etc/baud/host.conf`
so the host probe and operations tooling have a concrete configuration to check.
