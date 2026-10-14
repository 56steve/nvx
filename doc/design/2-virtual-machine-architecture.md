# Virtual Machine Architecture

[Architecture documentation](README.md)

The NVX machine profile presents a fixed Linux platform on every supported backend. The machine
profile is an allowlist: it constructs the required machine directly rather than creating a standard
PC and removing devices.

This chapter defines the backend-independent virtual platform presented to the guest.

## Table of Contents

- [2.1 Machine Model and Configuration](2.1-machine-model-and-configuration.md):
  Defines the machine identity, invariant configuration, and ownership of
  guest-visible settings.
- [2.2 CPU and Topology](2.2-cpu-and-topology.md): Describes the virtual
  processor model, topology, feature surface, and processor capacity.
- [2.3 Memory Layout](2.3-memory-layout.md): Defines RAM, reserved regions, MMIO
  apertures, and capacity behavior.
- [2.4 Interrupt and I/O Architecture](2.4-interrupt-and-io-architecture.md):
  Describes interrupt routing, port I/O, MMIO, and device notification.
- [2.5 Device Topology, Transport, and Discovery](2.5-device-topology-transport-and-discovery.md):
  Defines fixed device identities, addresses, interrupts, transport, and
  discovery.
