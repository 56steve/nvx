# NVX Architecture

This directory describes the architecture of the NVX microVM and its
single-workload guest runtime. It organizes the design by architectural
concept rather than by implementation component.

Implementation status, current limitations, validation procedures, test
coverage, and delivery plans are intentionally outside this architecture
guide.

## Contents

| Section | Description |
| --- | --- |
| [1. Architecture Overview](1-architecture-overview.md) | Introduces NVX's purpose, major architectural elements, trust boundaries, and compatibility principles. |
| [2. Virtual Machine Architecture](2-virtual-machine-architecture.md) | Defines the backend-independent virtual platform presented to the guest. |
| [3. Virtual Devices](3-virtual-devices.md) | Specifies the guest-visible behavior of each device class. |
| [4. Guest Runtime Architecture](4-guest-runtime-architecture.md) | Describes how the guest turns the virtual machine into an isolated workload sandbox. |
| [5. VM Lifecycle and State](5-vm-lifecycle-and-state.md) | Defines state transitions from creation through execution, capture, restoration, and termination. |
