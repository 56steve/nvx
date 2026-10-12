# Machine and device ABI

[Design index](../design.md)

## Architectural devices

The microVM base chipset contains exactly this allowlist:

- generic PIC and IOAPIC;
- the selected hypervisor's LAPIC;
- i8253 PIT on IRQ 0;
- generic programmable CMOS RTC anchored to UTC;
- raw bidirectional portb;
- status-carrying shutdown port; and
- guest snapshot-request port.

The RTC defaults to BCD, 24-hour fields with status B `0x02`. PIC, IOAPIC, PIT,
RTC, LAPIC, and VM time use common OpenVMM device and
state-unit machinery on every backend. IOAPIC saved state includes the
asserted level of every input line and reevaluates routing after restore, so a
level interrupt is neither lost nor treated as an edge while reconstructing
the backend. Serial UARTs, debugcon, Hyper-V power management, gameport, PCI,
firmware helpers, and standard-PC missing-port shims are absent.

The guest's clocks follow [NVX time ABI v1](time-abi.md) on every backend: a
minimal Hyper-V identity whose MSRs declare the TSC and LAPIC rates, served by
the `time-abi` state unit; the TSC as the only clocksource; the LAPIC timer in
one-shot mode at the backend's fixed rate (1 GHz on KVM, 200 MHz on MSHV and
WHP); and a PIT that the guest disables at boot. Capture rejects a periodic or
TSC-deadline LAPIC timer and a periodically counting PIT channel 0.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
flowchart TB
   Guest["x86-64 Linux guest<br/>direct boot, 1/2/4/8 vCPUs"]

   subgraph Abi["microVM machine contract"]
      direction LR
      Boot["Linux MP-table boot state<br/>and fixed RAM layout"]
      Interrupts["PIC, IOAPIC, LAPIC<br/>PIT, RTC, and VM time"]
      Pmio["PMIO devices<br/>portb, shutdown, snapshot"]
      Virtio["Fixed virtio-mmio<br/>net, fs, boot and control consoles,<br/>and versioned block roles"]
   end

   Common["OpenVMM worker<br/>state units and resource resolvers"]

   subgraph Backends["Execution backend"]
      direction LR
      KvmBackend["KVM"]
      MshvBackend["MSHV"]
      WhpBackend["WHP"]
   end

   Guest --> Boot
   Guest --> Interrupts
   Guest --> Pmio
   Guest --> Virtio
   Boot --> Common
   Interrupts --> Common
   Pmio --> Common
   Virtio --> Common
   Common --> KvmBackend
   Common --> MshvBackend
   Common --> WhpBackend
```

## PMIO devices

| Port | Device | Behavior |
| ---: | --- | --- |
| `0xe9` | portb data | Raw byte input and output; console reads consume one pending byte and zero-fill the remaining access width. With the restore packet or generation ID selected, a 1-, 2-, or 4-byte read returns that many bytes of the record, zero-filled past its end. |
| `0xea` | portb status | Bit 0 reports pending host input, bit 1 reports a fresh restore packet, bit 2 reports a processor target, bit 3 reports a memory target, bit 4 reports one or more memory-expansion ranges, bit 5 reports the fixed generation-ID selector, and bit 6 reports the time-sample window, which always exists. Writing `0xa5` after restore selects the one-time [restore packet](time-abi.md#restore-packet). Writing `0xa6` selects the current 16-byte generation ID; it may be selected repeatedly and remains stable for the lifetime of one VM process. Writing `0xa7` latches a fresh [time sample](time-abi.md#time-sample) into the window without changing the `0xe9` selection. |
| `0xeb` | portb time window | A 1-, 2-, or 4-byte read returns the next bytes of the latched time sample, zero-filled past its end or when none is latched. The window is not saved state. |
| `0x604` | shutdown | The first output byte becomes the process status carried with the VM power-off request. Reads return all ones. |
| `0x605` | snapshot request | Reads return all ones. Writes are coalesced and routed asynchronously to the capture controller. Zero requests fresh scratch and a nonzero first byte requests paired scratch. |

The portb receive and transmit buffers are each bounded at one MiB. Output
overflow drops the newest bytes and emits a rate-limited warning; input applies
backpressure by stopping host reads when its buffer is full. Pending bytes are
saved so capture does not silently lose VMM-owned I/O. While host input is
gated for a snapshot boundary or post-restore repair, portb stops reading its
host endpoint, returns zero for console data reads, and clears the
input-available status bit; guest output, the generation ID, any restore
packet, and the time-sample window remain available.

Guest-requested process exit drains the portb endpoint and, when present, its
host stdout relay before reporting completion. The combined drain has a
five-second deadline. Success preserves the guest's exit status; endpoint,
relay, or timeout failures become process-exit errors instead of silently
discarding final output. This is a portb/host-relay guarantee, not a general
drain of every virtio-console endpoint. Under the management RPC, a zero guest
status completes a pending wait and stops the server cleanly, while a nonzero
status or failed drain fails the pending wait and ends the server with an
error.

A snapshot-port write with no configured destination completes normally and
the guest continues. With a destination, the device permits at most one pending
transaction and defers completion long enough for the controller to establish
the exact post-`out` capture boundary. Repeated writes are coalesced. The PMIO
callback itself never pauses vCPUs, drains devices, hashes RAM, or writes files.
The scratch policy travels with the deferred boundary request. After a gated
restore, the same write acknowledges completion of guest repair.

## Fixed virtio-mmio transport

All eight fixed address slots are reserved, including the dedicated control
console at `0xd0007000..0xd0007fff` on IRQ 3 (shared status at `0x3001c`).
The window at `0xd0008000..0xd0008fff`, IRQ 13, and the shared-status word at
`0x30020` held a second virtio-fs slot in earlier releases; they are unused, and
a snapshot that recorded that slot no longer restores.
Every microVM cold-booted from the command line or the management RPC
instantiates the virtio-fs slot, so it is discoverable before capture even
without a host attachment; a restore keeps the slot only when the snapshot
recorded it. Several shares are children of one aggregate attachment on that
slot. Other optional devices are instantiated only when configured.
The control slot is instantiated for a control console, either a live
authenticated local endpoint on Linux or Windows or an explicitly disconnected
console. Every device uses virtio-mmio, is discovered only through
profile-owned command-line tokens, and has packed-ring support masked.
The MP table routes only the 16 ISA interrupts, and no device uses IRQ 13 on
KVM, MSHV, or WHP.

| Device | Stable identity | MMIO range | IRQ | Availability |
| --- | --- | ---: | ---: | --- |
| virtio-net | `net:microvm0` | `0xd0000000..0xd0000fff` | KVM/MSHV 10, WHP 5 | Optional |
| virtio-fs | `fs:microvm0` | `0xd0001000..0xd0001fff` | 6 | Reserved dormant slot; HostFs optional |
| virtio-console | `console:microvm-virtio0` | `0xd0002000..0xd0002fff` | 7 | Optional |
| `distro` virtio-blk | `blk:sandbox:distro` | `0xd0003000..0xd0003fff` | 4 | Optional read-only role |
| `runtime` virtio-blk | `blk:sandbox:runtime` | `0xd0004000..0xd0004fff` | 12 | Optional read-only role |
| `custom` virtio-blk | `blk:sandbox:custom` | `0xd0005000..0xd0005fff` | 9 | Optional read-only role |
| `scratch` virtio-blk | `blk:sandbox:scratch` | `0xd0006000..0xd0006fff` | 11 | Required writable final role when blocks are present |
| Control virtio-console | `console:microvm-control0` | `0xd0007000..0xd0007fff` | 3 | Optional authenticated local endpoint |

Explicit placement metadata bypasses the standard sequential MMIO allocator.
The worker validates the complete device count, kind, bus, address, IRQ, and
feature policy before resolving devices.

`--microvm-sandbox-block ROLE:DISK` assigns each attachment by
role rather than option order:

| Role | MMIO range | IRQ | Access |
| --- | ---: | ---: | --- |
| `distro` | `0xd0003000..0xd0003fff` | 4 | Read-only |
| `runtime` | `0xd0004000..0xd0004fff` | 12 | Read-only |
| `custom` | `0xd0005000..0xd0005fff` | 9 | Read-only |
| `scratch` | `0xd0006000..0xd0006fff` | 11 | Writable |

Roles must be unique and supplied in fixed order; omitted lower-layer roles
leave their slots empty, and any nonempty topology ends with scratch. All four
block IRQs use active-high edge delivery. IRQ 12 avoids the RTC's exclusive
IRQ 8.

The microVM keeps every fixed MMIO address and IRQ number and uses active-high edge
delivery for dedicated virtio IRQs. One little-endian, naturally aligned `u32`
per fixed slot resides in the reserved shared-status page:

| Slot | Status GPA |
| --- | ---: |
| virtio-net | `0x30000` |
| virtio-fs | `0x30004` |
| virtio-console | `0x30008` |
| `distro` block | `0x3000c` |
| `runtime` block | `0x30010` |
| `custom` block | `0x30014` |
| `scratch` block | `0x30018` |
| Control virtio-console | `0x3001c` |

OpenVMM publishes config-change and used-buffer bits with a sequentially
consistent compare-exchange loop. It pulses the device IRQ only when the old
word is zero. The specialized Linux driver consumes all pending bits with a
sequentially consistent `xchg` to zero, so steady-state handling performs no
interrupt-status read or interrupt-acknowledgement MMIO access. A host OR that
races the exchange is either included in the exchanged value or observes zero,
stores the bit, and emits a new edge. Device reset and teardown clear the word.
Snapshot quiesce saves the pending value in transport state; restore writes it
back without replaying an edge, and the machine contract records the interrupt
mode, page GPA, and page size.

### Block

Block devices are routed directly to virtio-mmio rather than VPCI. Packed rings
are unavailable. Each block has a stable role, MMIO address, IRQ, access mode, and
fixed feature mask. Its snapshot contract records the role, read-only flag,
logical length, logical and physical block sizes, and identity policy. Each
consumed block uses either whole-file SHA-256 or the caller-authenticated
immutable storage generation supplied at capture. Generation mode requires all
bound roles to carry the same nonzero generation and avoids content scans; the
storage controller remains responsible for binding that generation to the
exact immutable files. Platform-tier layers are recorded as unbound because
image binding has not been consumed; restore may supply different
same-geometry layers. Writable scratch uses one of two artifact policies:

- **paired**: capture publishes `scratch.img` with its exact length and selected
   identity. Restore uses the recorded `private-copy`, reflink-only
   `copy-on-write`, or single-use `direct-claimed` materialization policy;
- **fresh**: capture occurs before scratch is mounted, publishes no scratch
   artifact, and restore requires a new writable file with matching geometry.

Virtio transport and queue progress are saved with the other device state.
Stopping a queue prevents new descriptor intake and drains every accepted I/O
to one used-ring completion under the bounded snapshot quiesce timeout. The
controller flushes the exact scratch handle and copies it only after that drain.

### Console

The optional console is the standard single-port virtio-console device with
two split queues. Host RX accepted by the device and partial guest TX progress
are device-private saved state, so a descriptor is not replayed from byte zero
after restore. Native sockets and handles are not serialized.

The host attachment is a listener (a Unix socket, a named pipe, or loopback
TCP with a fixed nonzero port), a client connection, the inherited terminal,
or an explicitly disconnected endpoint that discards guest output. A listener
retains pending guest output until a client connects. The snapshot records the
attachment's stable ID, canonical endpoint identity, reconnect policy,
requiredness, and timeout. For a snapshot-capable machine, Unix sockets live
beside the snapshot directory and Windows pipes use OpenVMM's fixed microVM
pipe namespace. A saved listener may be recreated at a fresh private
restore-time endpoint, but its stable attachment ID, attachment kind, backend
kind, reconnect policy, required flag, length, and timeout remain exact.
Client reconnects retain their exact saved identity, require an explicitly
approved restore-time attachment, and have a five-second timeout; inherited
attachments must also be supplied again rather than serialized. Disconnected
attachments remain disconnected.

### Control console

The dedicated control console reuses the single-port virtio-console device but
has a distinct device and attachment kind, stable attachment ID
(`console:microvm-control0`), MMIO slot, IRQ, shared-status word, and
saved-state inventory. A fresh boot with a control console requires the boot
virtio-console, making the control device's guest tty `hvc2`; the profile owns
the identifying `nvx_control_tty=hvc2` token, and the boot console remains the
only kernel console.

When a control console is present, the command-line builder rejects
caller-supplied control-tty, driver-probe-order, and device-discovery tokens,
including kernel-equivalent hyphenated spellings, as well as quotes and the
`--` delimiter. These rules prevent guest tty discovery from being redirected
by command-line parsing. Boot-only command lines retain their existing
behavior.

The host endpoint is either a local listener or explicitly disconnected. A
live endpoint is protected in three layers:

- **Private endpoint.** On Linux, it is a Unix socket bound exclusively with
  mode `0600` inside an existing, non-symlink, owner-only directory of the
  OpenVMM user. On Windows, it is a named pipe whose protected DACL admits only
  LocalSystem and the OpenVMM user. TCP, client-connect, terminal, file,
  standard-stream, and inherited backends are rejected, and the boot and
  control endpoints must differ.
- **Peer identity.** The broker reserves its single host slot only for a peer
  whose operating-system identity is the OpenVMM user: the effective UID of
  the Unix-socket peer, or the token user SID of the process connected to the
  pipe. Any other peer is disconnected before its bytes are read.
- **Capability.** The launcher passes a random 32-byte capability through a
  prepared one-way standard-input pipe whose writers are closed before
  OpenVMM starts. The first host record must present it within a bounded
  authentication timeout, five seconds by default. The capability never
  appears in arguments, environment variables, logs, endpoint names,
  snapshots, or attachment identities. Because standard input carries it, the
  interactive console is disabled, the boot console cannot use the terminal,
  and the mode cannot be combined with the management RPC server, a console
  relay, or a paused start.

A disconnected control console uses a random capability that no client can
present.

OpenVMM brokers a frozen, versioned record stream between the guest and the
authenticated host. Every record has a fixed-size header with a magic value,
protocol version, record type, broker instance ID, epoch, direction-local
sequence, and payload length. Bootstrap attach records establish the guest and
host sides; acknowledgment, data, wait, ready, reset, authentication-error,
and credit records carry the session. Host data toward the guest is limited by
byte-counted receive credit that the guest grants. When credit is exhausted,
one complete host record may wait in a bounded pending slot, and each output
direction is bounded in records and bytes. Malformed lengths, wrong-state
records, wrong instance or epoch values, and duplicate or out-of-order
sequences fail closed. A wrong capability or an authentication timeout
detaches the client without advancing the epoch. An active host disconnect
advances the epoch, drops unstarted data and unused credit, finishes any
partially transmitted record to keep the stream aligned, and emits a reset;
the next session receives credit only from a fresh guest acknowledgment.

Snapshots record only the endpoint identity and a broker policy, either an
authenticated listener or a disconnected console, never the capability or the
peer UID or SID. Broker saved state omits queued records, host output, and
usable receive credit; restore keeps only the guest-side parser, any partially
transmitted guest record, and error counters. Restore validates the saved
endpoint and requires the launcher to request that same endpoint again with a
fresh capability. The restored broker receives a fresh instance ID, restarts at
the first epoch with a queued reset, and ignores guest records for the old
instance until the guest acknowledges the new one. The management RPC exposes
no control console and explicitly rejects restoring a snapshot that contains
one.

The control console carries the managed workload lifecycle: a host-owned
lifecycle token selects one-shot or managed operation, and managed operation
requires a fixed workload identity and a live, authenticated control console.
The workload protocol carried inside the brokered data stream belongs to the
guest agent; see
[Control protocol and checkpoint handoff](sandbox-filesystem-and-agent-architecture.md#control-protocol-and-checkpoint-handoff).

### Network

#### Architecture and device creation

Networking is a guest virtio-net NIC backed by the in-process Consomme
userspace network stack in OpenVMM. Consomme translates guest traffic into
ordinary host socket operations; it does not attach the guest to a physical
NIC, TAP interface, bridge, or host virtual switch. The host does not need a
network interface assigned the guest's gateway address.

The optional NIC occupies the fixed virtio-mmio slot at `0xd0000000`, has one
RX/TX queue pair, and advertises an exact feature mask: the MAC-address feature
and virtio version 1. OpenVMM constructs the virtual device and its Consomme
endpoint, binds the network identity and egress policy to them, and configures
any explicit host port forwards. The guest discovers the device through the
`virtio_mmio.device=` kernel command-line token.

`--net IPv4/prefix --network-profile portable` accepts prefixes `/1` through
`/30`, derives the first usable address as the gateway, and derives
deterministic guest and gateway MAC addresses from the final three IPv4
octets. The profile is mandatory and selects the same in-process Consomme data
plane on KVM, MSHV, and WHP; TAP attachments are rejected for this profile.

```mermaid
%%{init: {"theme": "base", "themeVariables": {"background": "#ffffff"}}}%%
flowchart TB
   Guest["Guest application and Linux TCP/IP stack"]
   Driver["Guest virtio-net driver"]
   Queues["TX/RX virtqueues in guest RAM<br/>MMIO notifications and interrupts"]
   Device["OpenVMM virtio-net device<br/>guest TX header prefilter"]
   Endpoint["In-process Consomme endpoint<br/>final guest TX authorization"]
   Host["Host sockets and DNS resolver"]
   Network["Host OS routing, firewall, and network"]
   Peer["Remote server or permitted host-local service"]

   Guest <--> Driver
   Driver <--> Queues
   Queues <--> Device
   Device <--> Endpoint
   Endpoint <--> Host
   Host <--> Network
   Network <--> Peer
```

#### Guest addressing and bootstrap

OpenVMM supplies the static IPv4 address, netmask, and gateway through the
`virtnet_ip=`, `virtnet_mask=`, and `virtnet_gw=` kernel command-line tokens.
The [guest initialization code](../../guest/common/init) configures the
interface and its default route from those tokens rather than using DHCP.

The NIC is dual-stack: its static IPv6 identity embeds the guest and gateway
IPv4 addresses in `fd00::/96`, with a prefix of 96 plus the IPv4 prefix, so
`10.0.0.2/24` becomes `fd00::a00:2/120` behind `fd00::a00:1`. The gateway
shares the IPv4 gateway's MAC address, and the guest configures the identity
from the `virtnet_ip6=` and `virtnet_gw6=` tokens without router
advertisements, autoconfiguration from them, or duplicate address detection.
For example:

| Setting | Value |
| --- | --- |
| Guest IPv4 address | `10.0.0.2/24` |
| IPv4 gateway | `10.0.0.1` |
| Guest IPv6 address | `fd00::a00:2/120` |
| IPv6 gateway | `fd00::a00:1` |

The guest kernel's EUI-64 link-local address is its only other IPv6 address,
and egress rules admit it only as the source of Neighbor Solicitations.
The static identity advertises no routable IPv6 prefix. External reachability
depends on the host's routes and address-family support; configuring this
private IPv6 identity does not give an IPv4-only host IPv6 Internet access.

#### Packet forwarding

Guest transmission follows this path:

1. The guest Linux stack constructs an Ethernet frame, and the virtio-net
   driver places its buffers in the TX virtqueue in guest RAM.
2. The driver notifies the virtual device through MMIO. OpenVMM reads the
   descriptors and prefilters the frame headers against the egress policy.
3. The endpoint copies the complete frame into an owned buffer, authorizes
   those exact bytes, and passes an allowed frame to the Consomme stack.
4. For an allowed new TCP connection, Consomme creates a host TCP socket and
   connects it to the destination. For UDP, it sends the guest's datagram
   payload through a host UDP socket.

TCP forwarding is socket proxying, not simply rewriting the original TCP
packet. Consomme implements the guest-facing TCP connection, while the host
kernel implements the remote-facing connection. Payload bytes move between
them with separate sequencing, acknowledgments, and buffering. A remote server
normally sees the host's network identity, not the guest's private address.
Host routing, VPN configuration, and applicable host firewall rules still
govern the host sockets; host kernel IP forwarding is not required.

When data arrives on a host socket, Consomme associates it with the
guest-facing flow and constructs the corresponding Ethernet/IP/TCP or UDP
packets. The endpoint writes them into buffers that the guest has posted on
the RX virtqueue. OpenVMM completes the descriptors and signals the guest
through the virtio interrupt mechanism; the guest driver passes the received
packets into Linux.

ARP and IPv6 Neighbor Discovery are handled locally by the backend, not
broadcast onto the physical LAN. The portable profile provides outbound TCP and
UDP over IPv4 and IPv6. The backend answers echo requests to the IPv4 or IPv6
gateway itself and sends other IPv4 echo requests through a host ICMP socket;
it does not relay ICMPv6 echo to other destinations. It deterministically
rejects fragmented IPv4 packets, IPv6 extension headers, and IPv4-mapped IPv6
destinations rather than forwarding unsupported packet constructions.

#### Egress policy and enforcement

By default, guest egress is allowed and general host ingress is denied. One
canonical egress policy applies before externally visible transmission:

- allow all traffic, the default;
- deny all traffic with `--network-egress deny` and no rules;
- allow only listed IPv4 hosts or CIDRs with `--allow-host`;
- allow IPv4 except listed hosts or CIDRs with `--block-host`;
- allow only exact IPv4 TCP endpoints with `--allow-endpoint`; or
- combine canonical IPv4 or IPv6 CIDR allow and deny rules with an optional
  TCP, UDP, or ICMP selector, optional TCP or UDP destination ports, and deny
  precedence.

The legacy `--allow-host`, `--block-host`, and `--allow-endpoint` modes are
mutually exclusive, cannot be combined with generic rules, and deny all IPv6
traffic. The generic rule mode requires an explicit `--network-egress allow`
or `--network-egress deny` default action and accepts at most 256 allow and
256 deny rules through `--network-egress-allow` and `--network-egress-deny`.
Rules select destinations by address or CIDR, with an optional transport and
TCP or UDP destination port or inclusive port range. Evaluation is independent
of rule order:

1. A matching deny rule denies the packet.
2. Otherwise, a matching allow rule allows it.
3. Otherwise, the default action decides.

A rule matches only destinations of its own address family. The IPv6 gateway's
Neighbor Discovery is authorized like the IPv4 gateway's ARP. Filtering is
inactive only when the policy allows all traffic: in the default mode, or with
generic rules that have an allow default and no deny rules. When filtering is
active, packet checks enforce the bound guest MAC and IP identity and validate
the headers, not just the destination. Port-specific traffic fails closed
when malformed or fragmented.

The virtio-net device validates the bound policy, installs it on the endpoint,
and performs the early header check. That check alone is insufficient because
guest RAM remains mutable. The endpoint therefore repeats authorization on
the complete owned frame before passing it to Consomme, so a guest cannot
rewrite a checked frame into a different transmitted destination. Denied
frames are dropped and their TX descriptors completed without host
transmission.

These are destination-based L3/L4 rules, not hostname or HTTP-content filters.
They apply to every frame that the guest transmits, including replies on
explicitly forwarded connections, but they are not an independent RX firewall
or host-wide firewall. Responses arriving through the host sockets use the RX
path.

#### Host-local access and ingress

By default, and subject to the egress policy, the gateway maps guest TCP and
UDP flows onto host loopback. The IPv4 gateway maps to `127.0.0.1`, and the
static IPv6 gateway maps to `::1`. For example, a guest connection to
`10.0.0.1:8080` reaches a host service on `127.0.0.1:8080`. The guest's own
`127.0.0.1` still means guest loopback, not host loopback. Guest flows to
other host-local-only destinations, such as link-local addresses, also reach
host sockets unless host loopback is denied.

`--host-loopback deny` disables the general gateway-to-host-loopback mappings,
blocks loopback, unspecified, and link-local destinations, and disallows every
forward. One exact IPv4 gateway TCP proxy endpoint may remain available under
deny through `--network-proxy`; it is bound into the policy digest, and
OpenVMM translates it to the same port on host `127.0.0.1`, where the
launcher, not OpenVMM, supplies the proxy service. The egress policy admits
TCP frames to that endpoint before evaluating its mode. Gateway DNS is a
separate backend service governed by the egress policy, not by the general
host-loopback mapping.

An outbound connection does not make guest services reachable from the host
or LAN. General `--network-ingress allow` is unsupported and rejected before
launch. Deliberate host-to-guest publishing instead requires explicit
localhost TCP or UDP port forwards, at most 64 of them. For example:

```text
--host-loopback allow
--host-loopback-forward tcp:8080:80
```

This binds a real host listener on `127.0.0.1:8080` and forwards connections
to guest port `80`; it does not bind a LAN-facing listener. Each forward
requires explicit `--host-loopback allow`, and omitting `--host-loopback`
keeps the default gateway mapping without publishing guest ports. The portable
NAT cannot offer generic bidirectional host-loopback connectivity, so an
explicit `--host-loopback allow` is accepted only with explicit
`--host-loopback-forward` attachments. For host-loopback peers, the backend
uses synthetic peer addresses so the guest routes replies through its NIC
rather than its own loopback, and reverse-translates those addresses on TX.

#### DNS

The gateway provides a DNS proxy over UDP and TCP port 53. The
`virtnet_dns=` token names the IPv4 gateway when the egress policy permits
DNS to it, and otherwise the IPv6 gateway when generic rules permit DNS to
that one. Exact-endpoint policies never name a gateway. If no gateway is
named, the token is omitted. The guest initialization code writes the
selected server into `/etc/resolv.conf`.

A guest query to the gateway passes the normal TX egress checks before
Consomme handles it as a DNS request. Upstream resolution uses the host
resolver. The glibc build that NVX uses with KVM calls the reentrant
`res_nsend` and forces TCP upstream for queries that arrived over TCP. The
statically linked musl build that NVX uses with MSHV calls `res_send`, which
selects its own transport. Both read the host's `/etc/resolv.conf`. Windows
calls `DnsQueryRaw` with the query's transport and the host DNS
configuration. Consomme relays the resolver's response from the gateway
through the guest RX path.

The host resolver's upstream DNS servers are not guest packet destinations,
so the guest's CIDR rules do not check them. Direct guest queries to an
external DNS server instead use the ordinary outbound socket path and require
permission for that server's address and port.

DNS permission and connection permission are separate: resolving a hostname
does not authorize a connection to the returned address, and the egress
policy does not filter domain names. Normal DNS behavior should permit both
UDP and TCP port 53 to the chosen gateway. Denying host-loopback access does
not by itself disable gateway DNS that the egress policy permits.

#### Bounded state, snapshots, and restore

Flow, DNS, buffer, and packet-queue state are bounded. At most 128 TCP, 256 UDP,
and 16 ICMP guest flows are active at once, and excess flows are rejected
before a host socket is created. At most 256 DNS requests may be pending.
A resolver failure produces a `SERVFAIL` response. At the pending-request
limit, a further UDP query receives `SERVFAIL`, and a further TCP query resets
its connection.

The snapshot records the profile, network identity, and policy digest. A
policy bound to an IPv6 identity uses canonical encoding version 4, while one
restored from a snapshot that predates IPv6 keeps version 3 and its recorded
digest. The snapshot records the IPv6 identity; a snapshot whose network
predates it restores IPv4-only. Restore reconstructs a fresh endpoint and
requires the profile and policy again; native sockets, DNS requests, and flow
objects are never serialized. Explicit localhost forwards are process-local
attachments, and snapshot capture and restore reject them.

Capture quiesces the endpoint, drains completion ownership, and requires the
saved queues to contain no unrepresented RX or TX packets. It then saves the
queue lifecycle, negotiated features, link state, and endpoint generation.
Arbitrary in-flight packet payloads are not serialized. Pre-capture queued
packets are not replayed, old TCP/UDP/ICMP/DNS state is invalidated, and new
traffic must establish fresh post-restore flows.

### Filesystem

The filesystem slot is a no-DAX virtio-fs device with the fixed tag `microvm`,
one high-priority queue, one request queue, direct I/O, and zero guest cache
lifetimes. It requires FUSE 7.31 or newer and caps writes at 1 MiB. Without
`--mount` or `--mount-aggregate`, the slot has no HostFs backend or active
filesystem policy but remains guest-discoverable. `--mount` may appear once and
attaches one HostFs server. `--mount-aggregate GUEST_TARGET` with a repeatable
`--mount-child NAME,HOST_PATH[,ro|rw][,file]` instead attaches an aggregate: a
synthetic root, mode `0500` and owned by root, which lists one child per host
directory or, with the `file` flag, regular file under its name and refuses
every change, and below each child an independent HostFs volume with its own
access mode and denied, allowed, and writable paths. The flag, not the host
object, sets a child's kind, and OpenVMM refuses a host path of the other kind.
A file child serves only its file, as the child
itself: its volume is the file's parent directory, narrowed to the file's name,
which the guest never reaches, so the guest cannot list or change anything
beside the file, a lookup below the file fails with `ENOTDIR`, and every
request fails with `EACCES` while anything but a regular file holds the name.
A file child takes no policy paths, because its mode applies to the whole file,
and restore requires each child to be the same kind. `--mount` exports only a
directory, because the guest mounts the device's root. Policy paths of an
aggregate are absolute and apply to the
child whose root contains them. Child names are unique `[A-Za-z0-9._-]`
names of at most 64 bytes, at most 256 children share at most 128 KiB of
policy paths, and their host roots must not equal or contain one another, so
no child can reach the files of another under a different policy, while a file
child may lie beside a directory child. A rename
between children fails with `EXDEV`. A hard link between children fails with
`EROFS` when its destination is read-only, which Linux also reports first, and
otherwise with `EXDEV`. The attachment adds one
`virtfs_dir=`, `virtfs_tag=`, `virtfs_mode=` token triplet, `rw` when any child
is read-write, and `virtfs_aggregate=1` for an aggregate. Its explicit profile
rejects SectionFs, alternate tags, extra queues, shared-memory windows, PCI
transport, and aggregates whose children change after boot.
The host root is pinned by its platform object identity and revalidated when
the export is opened, and every operation stays confined to the export: on
Linux, each host operation opens its parent directory without following a
symbolic link or leaving the root and never follows the final component; on
Windows, guest-created links are WSL-style reparse points that path resolution
never follows, and ancestor components that are links or reparse points are
rejected. A read-write mapping lets the guest create symbolic links with
exact targets, which only the guest resolves; a read-only mapping rejects
them. Bounded inode, alias, and handle tables fail an operation rather than
create untracked host state.
Read-only mode, of a share or of an aggregate child, rejects mutation in the
host device before invoking host filesystem operations; read-write mode exposes
only the supported common host contract.
Denied host paths are canonicalized into a bounded relative set and enforced
before HostFs operations. Prefix checks hide complete subtrees, while denied
root device/inode identities block hard-link, junction, and bind-mount
aliases. The policy is unchanged by a second guest mount.
Allowed host paths expose subtrees of denied paths again; the nearest policy
path that contains a path decides whether the guest can see it, and denied
and allowed paths alternate, so neither kind is redundant. A denied path may
name the root of a share or child when allowed paths inside it remain, which
leaves the root traverse-only. Policy path names may contain spaces, but not
begin or end with one. A hidden directory
on the way to an allowed path is traverse-only: the guest can look it up as a
directory and list only the entries that lead to allowed paths, and its object
identity is accepted only at its own path. Writable host paths, when present,
are the only parts of a read-write export that the guest can modify. Every
namespace mutation checks its directories and the entry paths that it
creates, removes, or replaces, and every object mutation checks every name
that the guest has used for the object; refused writes fail with `EROFS`, and
a hard link to a read-only object from a writable path fails with `EXDEV`.

The ownership mode selects the host identity of guest operations. By default,
HostFs performs them as the VMM. With `--mount-owner caller`, a Linux host
performs each request that can access host files as the UID and GID in its
FUSE header: the queue worker switches only its own thread's filesystem UID
and GID, clears the supplementary groups and effective capabilities, and
restores all of them before handling anything else. Session negotiation,
`FORGET`, and handle release run unchanged. Guest UID 0 and GID 0 are squashed
to the owner and group of the export root, which therefore must not be UID 0
or GID 0. A request whose identity cannot be assumed fails with `EPERM` rather
than running as the VMM. Windows hosts reject the mode.

The exported directory is external live state, not part of the VM snapshot.
An active capture saves its exact canonical host path, denied, allowed, and
writable paths, ownership mode, FUSE negotiation, node
and handle allocation, aliases (including those of symbolic links), lookup
counts, directory snapshots and cookies, and the identities needed to reopen
objects. Restore requires the same path,
target, mode, denied, allowed, and writable paths, ownership mode, root
identity, and reopenable objects for the captured attachment; for an
aggregate, the same target and the same children, names, kinds, modes, host
paths, root identities, and policies, in the same order, and the contract
records a
digest of the children's identities, so a restore can neither drop, add, nor
reorder one. The guest binds each child at a target that OpenVMM does not
record, so NVX names each child after its index and a digest of its guest
target: the snapshot then pins each target, and a restore that requests
another target fails before the guest runs. An aggregate's capture also saves
each child's volume and the synthetic root's directory state in
device-private schema version 8, or 9 when a child is a file, which earlier
releases reject instead of restoring the file child as a directory. A
caller-owned
capture records device-private schema version 6, which earlier releases reject
instead of restoring the attachment as the VMM. A capture with allowed or
writable paths records its complete access policy in device-private schema
version 7, which earlier releases reject instead of restoring the attachment
without its policy, and a restore rejects a handle open for writing outside
the writable paths. A dormant capture instead
saves explicit unattached state and may restore with no attachment or bind a
new HostFs backend. The resumed guest then mounts tag `microvm` explicitly;
the cold-boot mount hook has already run. Snapshots without this capability
cannot add an attachment. Native file descriptors and Windows handles are not
serialized.
