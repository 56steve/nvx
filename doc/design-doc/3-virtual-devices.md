# Virtual Devices

[Architecture documentation](README.md)

Virtual devices expose a small set of stable guest interfaces. Each interface has a fixed identity
and placement, bounded state, and explicit behavior during capture and restore.

This chapter specifies the guest-visible behavior of each device class.

## Table of Contents

- [3.1 Platform Control Device](3.1-platform-control-device.md): Covers the
  byte-channel, shutdown, and snapshot-request functions.
- [3.2 Clock and Time](3.2-clock-and-time.md): Defines clocksources, timer
  behavior, rates, and time continuity.
- [3.3 Consoles](3.3-consoles.md): Separates boot communication from
  authenticated runtime control.
- [3.4 Block Storage](3.4-block-storage.md): Defines fixed storage roles,
  layering, mutability, and saved state.
- [3.5 Network](3.5-network.md): Defines network identity, connectivity, policy,
  inbound access, and packet flow.
- [3.6 Host Filesystem](3.6-host-filesystem.md): Defines the virtio-fs request
  path, host-backed exports, access policy, and saved session state.
