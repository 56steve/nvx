//! Conformance checks C1 through C11 and the CI-only exhaustive check.

use std::io::Write;
use std::thread;
use std::time::Duration;

use super::decoders::{self, CpuInfo, ZERO_HYPERV_LEAVES};
use super::events::{DETAIL_MAX, escape_text};
use super::state::TimeState;
use super::system::{
    COMMAND_LINE_LIMIT, CPU_LIST_LIMIT, DAEMON_PID_PATH, DEFAULT_STALL_TIMEOUT_S, MsrDescriptor,
    Runtime, clock_ns, msr_error, rates_load, read_file, read_i64, read_text, state_apply,
    state_load,
};
use super::{MAX_CPUS, NSEC_PER_MSEC, NSEC_PER_SEC, Phase};
use crate::guest::status::CommandError;
use crate::guest::sys::x86::{cpuid, msr};
use crate::guest::sys::{self, Errno, MsrAccess, MsrFile, ProcessId, kernel};

/// Maximum timer-list convergence wait during boot.
const TIMER_LIST_WAIT_NS: i64 = 200 * NSEC_PER_MSEC;
/// Poll interval while the timer list converges.
const TIMER_LIST_POLL: Duration = Duration::from_millis(10);
/// Maximum wait for persisted phase checks to complete.
const STATUS_WAIT_NS: i64 = 30 * NSEC_PER_SEC;
/// Poll interval while waiting for persisted checks.
const STATUS_POLL_NS: i64 = 10 * NSEC_PER_MSEC;
/// Hz in a kHz, which `/proc/cpuinfo`'s rates use.
const HZ_PER_KHZ: u64 = 1000;
/// The process ID of init, which never runs the daemon.
const INIT_PROCESS_ID: i64 = 1;
/// The daemon's command name, as its procfs `comm` file holds it.
const DAEMON_COMMAND_NAME: &[u8] = b"nvx-time\n";

/// The hypervisor vendor signature of Hyper-V's CPUID leaf 0x40000000, "Microsoft Hv".
const HYPERV_VENDOR: [u32; 3] = [
    u32::from_le_bytes(*b"Micr"),
    u32::from_le_bytes(*b"osof"),
    u32::from_le_bytes(*b"t Hv"),
];
/// Last Hyper-V identity leaf that OpenVMM implements, which leaf 0x40000000 returns.
const HYPERV_MAXIMUM_LEAF: u32 = cpuid::LEAF_HYPERVISOR + 5;
/// The interface signature of Hyper-V's CPUID leaf 0x40000001, "Hv#1".
const HYPERV_INTERFACE: u32 = u32::from_le_bytes(*b"Hv#1");
/// The build of leaf 0x40000002, OpenVMM's NVX build: "NVX", then version 1.0 as the major
/// version in the upper half of EBX.
const HYPERV_BUILD: [u32; 2] = [u32::from_le_bytes(*b"NVX\0"), 1 << 16];
/// The partition privileges of leaf 0x40000003's EAX: the hypercall, VP-index, frequency, and
/// TSC invariant-control MSRs.
const HYPERV_PRIVILEGES: u32 = (1 << cpuid::HYPERV_EAX_HYPERCALL)
    | (1 << cpuid::HYPERV_EAX_VP_INDEX)
    | (1 << cpuid::HYPERV_EAX_ACCESS_FREQUENCY)
    | (1 << cpuid::HYPERV_EAX_ACCESS_TSC_INVARIANT);
/// The features of leaf 0x40000003's EDX: the frequency MSRs are available.
const HYPERV_FEATURES: u32 = 1 << cpuid::HYPERV_EDX_FREQUENCY_MSRS;
/// The spinlock-retry recommendation of leaf 0x40000004's EBX: never notify the hypervisor.
const HYPERV_NEVER_NOTIFY: u32 = u32::MAX;

/// The LAPIC timer rates that OpenVMM declares in the APIC-frequency MSR: 1 GHz or 200 MHz.
const LAPIC_RATES_HZ: [u64; 2] = [1_000_000_000, 200_000_000];
/// The value of the TSC invariant-control MSR that exposes the invariant TSC.
const TSC_INVARIANT_ENABLED: u64 = 1;
/// The invariant-control value that the MSR rejects, the first above its one valid bit.
const TSC_INVARIANT_REJECTED: u64 = 2;
/// The Hyper-V MSRs whose reads must fault, because OpenVMM does not implement them for NVX.
const FAULTING_READS: [u32; 3] = [
    msr::HV_GUEST_OS_ID,
    msr::HV_HYPERCALL,
    msr::HV_TIME_REF_COUNT,
];
/// The MSRs whose writes must fault in the exhaustive check.
const FAULTING_WRITES: [u32; 6] = [
    msr::HV_GUEST_OS_ID,
    msr::HV_HYPERCALL,
    msr::HV_VP_INDEX,
    msr::HV_TIME_REF_COUNT,
    msr::HV_TSC_FREQUENCY,
    msr::HV_APIC_FREQUENCY,
];
/// The architectural MSRs whose reads must fault in the exhaustive check.
const FAULTING_ARCHITECTURAL_READS: [u32; 2] = [msr::IA32_TSC_ADJUST, msr::IA32_TSC_DEADLINE];
/// The hypervisor leaves that the exhaustive check scans one by one, after the identity leaves.
const EXHAUSTIVE_LEAVES: (u32, u32, usize) = (0x4000_0006, 0x4000_00ff, 1);
/// The bases of the hypervisor ranges that the exhaustive check scans, such as KVM's.
const EXHAUSTIVE_BASES: (u32, u32, usize) = (0x4000_0100, 0x4000_ff00, 0x100);
/// The IDs of the exhaustive check's subchecks.
const EXHAUSTIVE_CHECKS: [&str; 6] = ["X1", "X2", "X3", "X4", "X5", "X6"];
/// The IDs of the exhaustive subchecks that read MSRs.
const EXHAUSTIVE_MSR_CHECKS: [&str; 4] = ["X3", "X4", "X5", "X6"];

/// The names of CPUID's four result registers, in the order that `cpuid` returns them.
const REGISTERS: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];
/// Position of EBX in a CPUID result.
const EBX: usize = 1;
/// Position of ECX in a CPUID result.
const ECX: usize = 2;
/// Position of EDX in a CPUID result.
const EDX: usize = 3;

/// One CPUID feature bit that the time ABI requires set or clear.
struct FeatureBit {
    /// The leaf that holds the bit.
    leaf: u32,
    /// The subleaf, for a leaf that has subleaves.
    subleaf: Option<u32>,
    /// The position of the register in the CPUID result.
    register: usize,
    /// The bit's number.
    bit: u32,
    /// The feature's name, which the diagnostic gives.
    name: &'static str,
    /// Whether the bit must be set.
    set: bool,
}

impl FeatureBit {
    /// Returns the bit's description, such as `CPUID.1:ECX[31] (hypervisor)`.
    fn label(&self) -> String {
        let subleaf = self
            .subleaf
            .map_or_else(String::new, |subleaf| format!(".{subleaf}"));
        format!(
            "CPUID.{:X}{subleaf}:{}[{}] ({})",
            self.leaf, REGISTERS[self.register], self.bit, self.name
        )
    }
}

/// The time bits of leaf 1 that the time ABI requires.
const LEAF1_TIME_BITS: [FeatureBit; 4] = [
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: ECX,
        bit: cpuid::FEATURES_ECX_HYPERVISOR,
        name: "hypervisor",
        set: true,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: ECX,
        bit: cpuid::FEATURES_ECX_TSC_DEADLINE,
        name: "TSC-deadline",
        set: false,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: ECX,
        bit: cpuid::FEATURES_ECX_PDCM,
        name: "PDCM",
        set: false,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: EDX,
        bit: cpuid::FEATURES_EDX_TSC,
        name: "TSC",
        set: true,
    },
];
/// Leaf 7's `IA32_TSC_ADJUST` bit, which the time ABI requires clear.
const TSC_ADJUST_BIT: FeatureBit = FeatureBit {
    leaf: cpuid::LEAF_STRUCTURED_FEATURES,
    subleaf: Some(0),
    register: EBX,
    bit: cpuid::STRUCTURED_FEATURES_EBX_TSC_ADJUST,
    name: "TSC_ADJUST",
    set: false,
};
/// Leaf 0x80000001's `RDTSCP` bit, which the time ABI requires set.
const RDTSCP_BIT: FeatureBit = FeatureBit {
    leaf: cpuid::LEAF_EXTENDED_FEATURES,
    subleaf: None,
    register: EDX,
    bit: cpuid::EXTENDED_FEATURES_EDX_RDTSCP,
    name: "RDTSCP",
    set: true,
};
/// Basic leaves whose every register the time ABI requires zero, when the processor has them:
/// performance monitoring, the TSC crystal ratio, and the frequencies.
const ZERO_BASIC_LEAVES: [u32; 3] = [
    cpuid::LEAF_PERFORMANCE_MONITORING,
    cpuid::LEAF_TSC_CRYSTAL,
    cpuid::LEAF_FREQUENCY,
];

/// Formats a CPUID result as the checks print it.
fn registers(value: [u32; 4]) -> String {
    format!(
        "{:08x}:{:08x}:{:08x}:{:08x}",
        value[0], value[1], value[2], value[3]
    )
}

#[derive(Clone)]
/// Mutable evidence and failure count for one conformance-check phase.
pub(crate) struct Checks {
    /// Phase whose failures are being reported.
    pub(crate) phase: Phase,
    /// Number of failed checks.
    pub(crate) failures: i32,
    /// Number of possible CPUs.
    pub(crate) possible: usize,
    /// Number of currently online CPUs.
    pub(crate) online_count: usize,
    /// Fixed online-CPU mask.
    pub(crate) online: [bool; MAX_CPUS],
    /// Parsed per-CPU frequency evidence.
    pub(crate) cpuinfo: CpuInfo,
    /// Validated TSC frequency.
    pub(crate) tsc_hz: u64,
    /// Validated LAPIC timer frequency.
    pub(crate) lapic_hz: u64,
}

impl Checks {
    /// Creates an empty check set for `phase`.
    pub(crate) fn empty(phase: Phase) -> Self {
        Self {
            phase,
            failures: 0,
            possible: 0,
            online_count: 0,
            online: [false; MAX_CPUS],
            cpuinfo: CpuInfo::default(),
            tsc_hz: 0,
            lapic_hz: 0,
        }
    }

    /// Loads and validates the possible and online CPU masks.
    pub(crate) fn load(phase: Phase) -> Result<Self, Errno> {
        let possible = read_text(kernel::POSSIBLE_CPUS, CPU_LIST_LIMIT)?;
        let online = read_text(kernel::ONLINE_CPUS, CPU_LIST_LIMIT)?;
        let (_, possible_count) = decoders::parse_cpu_list(&possible).ok_or(Errno::EINVAL)?;
        let (online, online_count) = decoders::parse_cpu_list(&online).ok_or(Errno::EINVAL)?;
        if possible_count == 0 || online_count == 0 {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            possible: possible_count,
            online_count,
            online,
            ..Self::empty(phase)
        })
    }

    /// Records one failure under an explicit ABI code.
    pub(crate) fn fail_code(
        &mut self,
        runtime: &Runtime<'_>,
        code: &[u8],
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.failures = self.failures.saturating_add(1);
        runtime.conformance_failure(code, self.phase, detail.as_ref());
        false
    }

    /// Records one `G_CONFORMANCE_Cn` failure.
    pub(crate) fn fail(
        &mut self,
        runtime: &Runtime<'_>,
        id: &str,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.fail_code(runtime, format!("G_CONFORMANCE_{id}").as_bytes(), detail)
    }
}

/// Publishes one completed phase's status, timing, failures, and rates.
pub(crate) fn record_check(
    runtime: &Runtime<'_>,
    checks: &Checks,
    status: &[u8],
    elapsed_us: i64,
    cpu_us: i64,
    tsc_hz: u64,
    lapic_hz: u64,
) -> Result<(), Errno> {
    let Some(index) = checks.phase.check_index() else {
        return Err(Errno::EINVAL);
    };
    state_apply(|state| {
        let check = &mut state.checks[index];
        check.status = status.to_vec();
        check.cpus = checks.online_count as u64;
        check.elapsed_us = elapsed_us;
        check.cpu_us = cpu_us;
        check.generation = if checks.phase == Phase::Boot {
            0
        } else {
            state.generation
        };
        check.failures = -1;
        if runtime.report_only() {
            check.failures = i64::from(checks.failures);
            if checks.failures > 0 && check.status == b"ok" {
                check.status = b"fail".to_vec();
            }
        }
        if tsc_hz != 0 {
            state.tsc_hz = tsc_hz;
        }
        if lapic_hz != 0 {
            state.lapic_hz = lapic_hz;
        }
    })
}

/// Validates the Hyper-V CPUID identity leaves on one CPU.
fn check_identity(runtime: &Runtime<'_>, checks: &mut Checks, cpu: usize) -> bool {
    let capacity = checks.possible as u32;
    let expected = [
        [
            HYPERV_MAXIMUM_LEAF,
            HYPERV_VENDOR[0],
            HYPERV_VENDOR[1],
            HYPERV_VENDOR[2],
        ],
        [HYPERV_INTERFACE, 0, 0, 0],
        [HYPERV_BUILD[0], HYPERV_BUILD[1], 0, 0],
        [HYPERV_PRIVILEGES, 0, 0, HYPERV_FEATURES],
        [0, HYPERV_NEVER_NOTIFY, 0, 0],
        [capacity, capacity, 0, 0],
    ];
    for (leaf, expected) in (cpuid::LEAF_HYPERVISOR..).zip(expected) {
        let value = sys::x86::cpuid(leaf, 0);
        if value != expected {
            return checks.fail(
                runtime,
                "C1",
                format!(
                    "cpu {cpu} leaf 0x{leaf:08x} is {}, expected {}",
                    registers(value),
                    registers(expected)
                ),
            );
        }
    }
    for leaves in ZERO_HYPERV_LEAVES {
        for leaf in leaves {
            let value = sys::x86::cpuid(leaf, 0);
            if value != [0; 4] {
                return checks.fail(
                    runtime,
                    "C1",
                    format!(
                        "cpu {cpu} leaf 0x{leaf:08x} is {}, expected zero",
                        registers(value)
                    ),
                );
            }
        }
    }
    true
}

/// Validates one CPUID feature bit in `value`, the result of its leaf.
fn check_bit(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    cpu: usize,
    bit: &FeatureBit,
    value: [u32; 4],
) -> bool {
    if cpuid::bit(value[bit.register], bit.bit) == bit.set {
        true
    } else {
        checks.fail(
            runtime,
            "C2",
            format!(
                "cpu {cpu} {} is {}",
                bit.label(),
                if bit.set { "clear" } else { "set" }
            ),
        )
    }
}

/// Validates one complete CPUID leaf value.
fn check_leaf(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    cpu: usize,
    leaf: u32,
    expected: [u32; 4],
) -> bool {
    let value = sys::x86::cpuid(leaf, 0);
    if value == expected {
        true
    } else {
        checks.fail(
            runtime,
            "C2",
            format!(
                "cpu {cpu} leaf 0x{leaf:08x} is {}, expected {}",
                registers(value),
                registers(expected)
            ),
        )
    }
}

/// Validates time-related architectural and Hyper-V feature bits.
fn check_time_bits(runtime: &Runtime<'_>, checks: &mut Checks, cpu: usize) -> bool {
    let maximum = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0)[0];
    let features = sys::x86::cpuid(cpuid::LEAF_FEATURES, 0);
    if !LEAF1_TIME_BITS
        .iter()
        .all(|bit| check_bit(runtime, checks, cpu, bit, features))
    {
        return false;
    }
    if maximum < cpuid::LEAF_POWER_MANAGEMENT {
        return checks.fail(
            runtime,
            "C2",
            format!(
                "cpu {cpu} lacks leaf {:#x} (ARAT)",
                cpuid::LEAF_POWER_MANAGEMENT
            ),
        );
    }
    let arat = [1 << cpuid::POWER_MANAGEMENT_EAX_ARAT, 0, 0, 0];
    if !check_leaf(runtime, checks, cpu, cpuid::LEAF_POWER_MANAGEMENT, arat) {
        return false;
    }
    if maximum >= cpuid::LEAF_STRUCTURED_FEATURES {
        let structured = sys::x86::cpuid(cpuid::LEAF_STRUCTURED_FEATURES, 0);
        if !check_bit(runtime, checks, cpu, &TSC_ADJUST_BIT, structured) {
            return false;
        }
    }
    for leaf in ZERO_BASIC_LEAVES {
        if maximum >= leaf && !check_leaf(runtime, checks, cpu, leaf, [0; 4]) {
            return false;
        }
    }
    if sys::x86::cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0)[0] < cpuid::LEAF_ADVANCED_POWER {
        return checks.fail(
            runtime,
            "C2",
            format!("cpu {cpu} lacks leaf {:#x}", cpuid::LEAF_ADVANCED_POWER),
        );
    }
    let extended = sys::x86::cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0);
    if !check_bit(runtime, checks, cpu, &RDTSCP_BIT, extended) {
        return false;
    }
    let invariant_tsc = [0, 0, 0, 1 << cpuid::ADVANCED_POWER_EDX_INVARIANT_TSC];
    check_leaf(
        runtime,
        checks,
        cpu,
        cpuid::LEAF_ADVANCED_POWER,
        invariant_tsc,
    )
}

/// Validates required, rate, invariant-control, and faulting MSRs.
fn check_msrs(runtime: &Runtime<'_>, checks: &mut Checks, cpu: usize, full: bool) -> bool {
    let descriptor = match runtime.ops.open_msr(cpu, MsrAccess::Read) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return checks.fail(
                runtime,
                "C3",
                format!("cpu {cpu}: open {}: {}", MsrFile::path(cpu), error.desc()),
            );
        }
    };
    let mut ok = true;
    if full {
        let result = runtime.ops.read_msr(&descriptor, msr::HV_VP_INDEX);
        let value = *result.as_ref().unwrap_or(&0);
        if result.is_err() || value != cpu as u64 {
            ok = checks.fail(
                runtime,
                "C3",
                format!(
                    "cpu {cpu}: MSR {:#010x} (VP index) is {value} (error {})",
                    msr::HV_VP_INDEX,
                    msr_error(&result)
                ),
            );
        }
    }
    if cpu != 0 {
        return ok;
    }
    if ok {
        match runtime.ops.read_msr(&descriptor, msr::HV_TSC_FREQUENCY) {
            Err(error) => {
                ok = checks.fail(
                    runtime,
                    "C3",
                    format!(
                        "cpu {cpu}: MSR {:#010x} read: {}",
                        msr::HV_TSC_FREQUENCY,
                        error.desc()
                    ),
                );
            }
            Ok(value) if checks.tsc_hz != 0 && value != checks.tsc_hz => {
                ok = checks.fail(
                    runtime,
                    "C3",
                    format!(
                        "cpu {cpu}: MSR {:#010x} is {value} Hz, expected {} Hz",
                        msr::HV_TSC_FREQUENCY,
                        checks.tsc_hz
                    ),
                );
            }
            Ok(value) => checks.tsc_hz = value,
        }
    }
    if ok && full {
        let result = runtime.ops.read_msr(&descriptor, msr::HV_APIC_FREQUENCY);
        let value = *result.as_ref().unwrap_or(&0);
        if result.is_err()
            || !LAPIC_RATES_HZ.contains(&value)
            || (checks.lapic_hz != 0 && value != checks.lapic_hz)
        {
            ok = checks.fail(
                runtime,
                "C3",
                format!(
                    "cpu {cpu}: MSR {:#010x} (LAPIC rate) is {value} (error {})",
                    msr::HV_APIC_FREQUENCY,
                    msr_error(&result)
                ),
            );
        } else {
            checks.lapic_hz = value;
        }
    }
    if ok && full {
        let result = runtime
            .ops
            .read_msr(&descriptor, msr::HV_TSC_INVARIANT_CONTROL);
        let value = *result.as_ref().unwrap_or(&0);
        if result.is_err() || value != TSC_INVARIANT_ENABLED {
            ok = checks.fail(
                runtime,
                "C3",
                format!(
                    "cpu {cpu}: MSR {:#010x} is {value} (error {})",
                    msr::HV_TSC_INVARIANT_CONTROL,
                    msr_error(&result)
                ),
            );
        }
    }
    for register in FAULTING_READS {
        if ok && full {
            let result = runtime.ops.read_msr(&descriptor, register);
            if result != Err(Errno::EIO) {
                ok = checks.fail(
                    runtime,
                    "C3",
                    format!(
                        "cpu {cpu}: MSR {register:#010x} read did not fail with EIO (error {})",
                        msr_error(&result)
                    ),
                );
            }
        }
    }
    ok
}

/// Validates `/proc/cpuinfo` for selected CPUs.
fn check_cpuinfo(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    selected: &[bool; MAX_CPUS],
    flags: bool,
) -> bool {
    let text = match read_file(kernel::CPU_INFORMATION) {
        Ok(text) => text,
        Err(error) => {
            return checks.fail(
                runtime,
                "C5",
                format!(
                    "read {}: {}",
                    kernel::CPU_INFORMATION.to_string_lossy(),
                    error.desc()
                ),
            );
        }
    };
    let (info, result) = decoders::check_cpuinfo(&text, selected, flags);
    checks.cpuinfo = info;
    match result {
        Ok(()) => true,
        Err(detail) => checks.fail(runtime, "C5", detail),
    }
}

/// Validates current and available clocksources from sysfs.
fn check_clocksource(runtime: &Runtime<'_>, checks: &mut Checks) -> bool {
    let sources = read_file(kernel::CURRENT_CLOCKSOURCE).and_then(|current| {
        read_file(kernel::AVAILABLE_CLOCKSOURCES).map(|available| (current, available))
    });
    let (current, available) = match sources {
        Ok(sources) => sources,
        Err(error) => {
            return checks.fail(
                runtime,
                "C6",
                format!("clocksource sysfs: {}", error.desc()),
            );
        }
    };
    match decoders::check_clocksource(&current, &available) {
        Ok(()) => true,
        Err(detail) => checks.fail(runtime, "C6", detail),
    }
}

/// Waits for and validates `/proc/timer_list`.
pub(crate) fn check_timer_list(runtime: &Runtime<'_>, checks: &mut Checks, wait_ns: i64) -> bool {
    let deadline = clock_ns(sys::Clock::Monotonic).wrapping_add(wait_ns);
    loop {
        let text = match read_file(kernel::TIMER_LIST) {
            Ok(text) => text,
            Err(error) => {
                return checks.fail(
                    runtime,
                    "C7",
                    format!(
                        "read {}: {}",
                        kernel::TIMER_LIST.to_string_lossy(),
                        error.desc()
                    ),
                );
            }
        };
        match decoders::timer_list_verdict(&text, &checks.online) {
            Ok(()) => return true,
            Err(detail) if clock_ns(sys::Clock::Monotonic) >= deadline => {
                return checks.fail(runtime, "C7", detail);
            }
            Err(_) => thread::sleep(TIMER_LIST_POLL),
        }
    }
}

/// Validates absence of unsupported platform devices and RCU settings.
fn check_platform(runtime: &Runtime<'_>, checks: &mut Checks) -> bool {
    if sys::path_exists(kernel::VMBUS) {
        return checks.fail(
            runtime,
            "C8",
            format!("{} exists", kernel::VMBUS.to_string_lossy()),
        );
    }
    if sys::path_exists(kernel::CPU_FREQUENCY_POLICY) {
        return checks.fail(runtime, "C8", "cpufreq policy0 exists");
    }
    let suppress = read_i64(kernel::RCU_STALL_SUPPRESS).unwrap_or(-1);
    if suppress != 0 {
        return checks.fail(
            runtime,
            "C8",
            format!("rcu_cpu_stall_suppress is {suppress}"),
        );
    }
    let timeout = read_i64(kernel::RCU_STALL_TIMEOUT).unwrap_or(-1);
    if timeout != DEFAULT_STALL_TIMEOUT_S {
        return checks.fail(runtime, "C8", format!("rcu_cpu_stall_timeout is {timeout}"));
    }
    true
}

/// Validates the guest kernel command line.
fn check_cmdline(runtime: &Runtime<'_>, checks: &mut Checks) -> bool {
    let text = match read_text(kernel::COMMAND_LINE, COMMAND_LINE_LIMIT) {
        Ok(text) => text,
        Err(error) => {
            return checks.fail(
                runtime,
                "C9",
                format!(
                    "read {}: {}",
                    kernel::COMMAND_LINE.to_string_lossy(),
                    error.desc()
                ),
            );
        }
    };
    match decoders::check_cmdline(&text) {
        Ok(()) => true,
        Err(detail) => checks.fail(runtime, "C9", detail),
    }
}

/// Finds and validates the published `nvx-time` daemon process.
pub(crate) fn daemon_pid() -> Option<ProcessId> {
    let pid = read_i64(DAEMON_PID_PATH).ok()?;
    let process = ProcessId::from_positive(pid).filter(|_| pid > INIT_PROCESS_ID)?;
    if !sys::process_exists(process) {
        return None;
    }
    (sys::process_name(process).ok()?.as_slice() == DAEMON_COMMAND_NAME).then_some(process)
}

/// Validates daemon liveness, state, violations, and RCU stalls.
fn check_daemon(runtime: &Runtime<'_>, checks: &mut Checks) -> bool {
    if daemon_pid().is_none() {
        return checks.fail(runtime, "C10", "the time daemon is not running");
    }
    let state = match state_load() {
        Ok(state) => state,
        Err(_) => return checks.fail(runtime, "C10", "time state is unreadable"),
    };
    if state.violations != 0 {
        return checks.fail(
            runtime,
            "C10",
            format!("the time daemon recorded {} violations", state.violations),
        );
    }
    let stalls = read_i64(kernel::RCU_STALL_COUNT).unwrap_or(-1);
    if stalls != 0 {
        return checks.fail(runtime, "C10", format!("rcu_stall_count is {stalls}"));
    }
    true
}

/// Validates soft and hung-task watchdog configuration when present.
fn check_debug_watchdogs(runtime: &Runtime<'_>, checks: &mut Checks) -> bool {
    let soft = sys::path_exists(kernel::SOFT_WATCHDOG);
    let hung = sys::path_exists(kernel::HUNG_TASK_TIMEOUT);
    if !soft && !hung {
        return true;
    }
    let value = if soft {
        read_i64(kernel::SOFT_WATCHDOG).unwrap_or(-1)
    } else {
        -1
    };
    if !soft || value != 1 {
        return checks.fail(runtime, "C11", format!("soft_watchdog is {value}"));
    }
    let value = if hung {
        read_i64(kernel::HUNG_TASK_TIMEOUT).unwrap_or(-1)
    } else {
        -1
    };
    if !hung || value == 0 {
        return checks.fail(runtime, "C11", format!("hung_task_timeout_secs is {value}"));
    }
    true
}

/// Runs selected identity and MSR checks on one pinned CPU.
fn check_one_cpu(
    runtime: &Runtime<'_>,
    base: &Checks,
    cpu: usize,
    identity: bool,
    msrs: bool,
) -> Checks {
    let mut checks = base.clone();
    checks.failures = 0;
    if let Err(error) = sys::pin_current_thread(cpu) {
        checks.fail(
            runtime,
            "C1",
            format!("cannot run on cpu {cpu}: {}", error.desc()),
        );
        return checks;
    }
    if identity {
        check_identity(runtime, &mut checks, cpu);
        check_time_bits(runtime, &mut checks, cpu);
    }
    if msrs {
        check_msrs(runtime, &mut checks, cpu, true);
    }
    checks
}

/// Runs selected per-CPU checks on this thread, which returns to its original affinity.
fn check_cpus(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    selected: &[bool; MAX_CPUS],
    identity: bool,
    msrs: bool,
) -> bool {
    let _affinity = match sys::AffinityGuard::save() {
        Ok(affinity) => affinity,
        Err(error) => {
            return checks.fail(
                runtime,
                "C1",
                format!("sched_getaffinity: {}", error.desc()),
            );
        }
    };
    let mut ok = true;
    for (cpu, selected) in selected.iter().copied().enumerate() {
        if !selected || !ok {
            continue;
        }
        let child = check_one_cpu(runtime, checks, cpu, identity, msrs);
        checks.failures += child.failures;
        if cpu == 0 && msrs {
            checks.tsc_hz = child.tsc_hz;
            checks.lapic_hz = child.lapic_hz;
        }
        ok = child.failures == 0;
    }
    ok
}

/// The kernel log that the boot checks read from a buffer allocated before they start.
pub(crate) struct BootLog<'a> {
    /// The log's size, as `klogctl` returned it, which a diagnostic gives.
    pub(crate) size: i64,
    /// The buffer, unless it could not be allocated.
    pub(crate) buffer: Option<&'a mut [u8]>,
}

/// Validates boot-log evidence from a preallocated kernel-log buffer.
fn boot_log(runtime: &Runtime<'_>, checks: &mut Checks, log: Option<BootLog<'_>>) {
    let Some(BootLog { size, buffer }) = log else {
        return;
    };
    let Some(buffer) = buffer else {
        checks.fail(
            runtime,
            "C4",
            format!("no buffer for the {size}-byte kernel log"),
        );
        return;
    };
    let count = match sys::kernel::read_kernel_log(buffer) {
        Ok(count) => count,
        Err(error) => {
            checks.fail(
                runtime,
                "C4",
                format!("read the kernel log: {}", error.desc()),
            );
            return;
        }
    };
    let log = decoders::scan_boot_log(&buffer[..count], true);
    if let Err(detail) = log.verdict() {
        checks.fail(runtime, "C4", detail);
    }
    if !log.wx.is_empty() {
        checks.fail_code(runtime, b"G_KERNEL_WX", &log.wx);
    }
}

/// Runs a side check on a copy of `base` that starts without failures, and returns the copy.
fn side_check(base: &Checks, check: impl FnOnce(&mut Checks)) -> Checks {
    let mut side = base.clone();
    side.failures = 0;
    check(&mut side);
    side
}

/// Runs every boot check except C12 in the same per-CPU and side-job parallel shape as C. With
/// one online CPU, every check runs on this thread.
pub(crate) fn run_boot_checks(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    log: Option<BootLog<'_>>,
) {
    let base = checks.clone();
    let selected = checks.online;
    if checks.online_count <= 1 {
        check_cmdline(runtime, checks);
        check_platform(runtime, checks);
        check_clocksource(runtime, checks);
        check_debug_watchdogs(runtime, checks);
        checks.failures += side_check(&base, |side| boot_log(runtime, side, log)).failures;
        checks.failures += side_check(&base, |side| {
            check_timer_list(runtime, side, TIMER_LIST_WAIT_NS);
        })
        .failures;
        let cpuinfo = side_check(&base, |side| {
            check_cpuinfo(runtime, side, &selected, true);
        });
        checks.failures += cpuinfo.failures;
        checks.cpuinfo = cpuinfo.cpuinfo;
        check_cpus(runtime, checks, &selected, true, true);
        compare_cpu_rate(runtime, checks);
        return;
    }
    thread::scope(|scope| {
        let log = scope.spawn(|| side_check(&base, |side| boot_log(runtime, side, log)));
        let timer = scope.spawn(|| {
            side_check(&base, |side| {
                check_timer_list(runtime, side, TIMER_LIST_WAIT_NS);
            })
        });
        let cpuinfo = scope.spawn(|| {
            side_check(&base, |side| {
                check_cpuinfo(runtime, side, &selected, true);
            })
        });
        let cpus = selected
            .iter()
            .enumerate()
            .filter(|(_, selected)| **selected)
            .map(|(cpu, _)| {
                let base = &base;
                (
                    cpu,
                    scope.spawn(move || check_one_cpu(runtime, base, cpu, true, true)),
                )
            })
            .collect::<Vec<_>>();

        check_cmdline(runtime, checks);
        check_platform(runtime, checks);
        check_clocksource(runtime, checks);
        check_debug_watchdogs(runtime, checks);

        for side in [log.join(), timer.join()] {
            match side {
                Ok(side) => checks.failures += side.failures,
                Err(_) => {
                    checks.fail(runtime, "C10", "a boot side job crashed");
                }
            }
        }
        match cpuinfo.join() {
            Ok(side) => {
                checks.failures += side.failures;
                checks.cpuinfo = side.cpuinfo;
            }
            Err(_) => {
                checks.fail(runtime, "C10", "the cpuinfo side job crashed");
            }
        }
        for (cpu, handle) in cpus {
            match handle.join() {
                Ok(child) => {
                    checks.failures += child.failures;
                    if cpu == 0 {
                        checks.tsc_hz = child.tsc_hz;
                        checks.lapic_hz = child.lapic_hz;
                    }
                }
                Err(_) => {
                    checks.fail(runtime, "C1", format!("the cpu {cpu} job crashed"));
                }
            }
        }
    });
    compare_cpu_rate(runtime, checks);
}

/// Cross-checks the validated TSC rate against CPU zero's cpuinfo rate.
fn compare_cpu_rate(runtime: &Runtime<'_>, checks: &mut Checks) {
    if checks.tsc_hz != 0
        && checks.cpuinfo.known[0]
        && checks.tsc_hz / HZ_PER_KHZ != checks.cpuinfo.khz[0]
    {
        checks.fail(
            runtime,
            "C3",
            format!(
                "cpu 0: floor(F / 1000) is {} kHz but cpu MHz is {} kHz",
                checks.tsc_hz / HZ_PER_KHZ,
                checks.cpuinfo.khz[0]
            ),
        );
    }
}

/// Runs checks that must precede snapshot capture.
pub(crate) fn run_capture_checks(runtime: &Runtime<'_>, checks: &mut Checks) {
    if let Ok((tsc, lapic)) = rates_load() {
        checks.tsc_hz = tsc;
        checks.lapic_hz = lapic;
    }
    check_clocksource(runtime, checks);
    check_timer_list(runtime, checks, 0);
    check_daemon(runtime, checks);
}

/// Runs restore checks, including checks on newly online CPUs.
pub(crate) fn run_restore_checks(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    new_cpus: &[bool; MAX_CPUS],
    new_count: usize,
) {
    match rates_load() {
        Ok((tsc, lapic)) => {
            checks.tsc_hz = tsc;
            checks.lapic_hz = lapic;
        }
        Err(_) => {
            checks.fail(runtime, "C3", "the boot rates are unavailable");
        }
    }
    if new_count > 0 {
        check_cpus(runtime, checks, new_cpus, true, false);
        check_cpuinfo(runtime, checks, new_cpus, true);
    }
    check_msrs(runtime, checks, 0, false);
    check_clocksource(runtime, checks);
    check_daemon(runtime, checks);
}

/// Waits for boot-only or all persisted checks and returns completion state.
pub(crate) fn wait_for_checks(boot_only: bool) -> (TimeState, bool) {
    let deadline = clock_ns(sys::Clock::Monotonic).wrapping_add(STATUS_WAIT_NS);
    loop {
        let state = state_load().unwrap_or_default();
        let count = if boot_only { 1 } else { Phase::CHECKS.len() };
        let pending = state.checks[..count]
            .iter()
            .any(|check| check.status == b"pending");
        if !pending {
            return (state, true);
        }
        if clock_ns(sys::Clock::Monotonic) >= deadline {
            return (state, false);
        }
        thread::sleep(Duration::from_nanos(STATUS_POLL_NS as u64));
    }
}

/// Aggregate counters and rates for the exhaustive CI-only report.
struct Exhaustive {
    /// Number of failed exhaustive subchecks.
    failures: i32,
    /// First validated TSC rate.
    tsc_hz: u64,
    /// First validated LAPIC rate.
    lapic_hz: u64,
    /// Parsed CPU frequency evidence.
    cpuinfo: CpuInfo,
}

/// Emits one stable exhaustive-check report line and updates failures.
fn exhaustive_report(
    output: &mut dyn Write,
    state: &mut Exhaustive,
    id: &str,
    cpu: usize,
    passed: bool,
    detail: &[u8],
) {
    let escaped = escape_text(detail, DETAIL_MAX - 1);
    let _ = write!(
        output,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 check={id} cpu={cpu} status={} detail=\"",
        if passed { "pass" } else { "fail" }
    );
    let _ = output.write_all(&escaped);
    let _ = output.write_all(b"\"\n");
    if !passed {
        state.failures += 1;
    }
}

/// Scans a sparse CPUID leaf range, `(first, last, step)`, and emits one aggregate result.
fn exhaustive_leaves(
    output: &mut dyn Write,
    state: &mut Exhaustive,
    id: &str,
    cpu: usize,
    (first, last, step): (u32, u32, usize),
    out_of_range: [u32; 4],
) {
    let mut first_detail = String::new();
    let mut failing = 0;
    let mut count = 0;
    for leaf in (first..=last).step_by(step) {
        count += 1;
        if let Err(detail) =
            decoders::exhaustive_leaf_ok(leaf, sys::x86::cpuid(leaf, 0), out_of_range)
        {
            if failing == 0 {
                first_detail = detail;
            }
            failing += 1;
        }
    }
    let detail = if failing == 0 {
        format!(
            "{count} {} read zero or the highest basic leaf",
            if step == 1 { "leaves" } else { "bases" }
        )
    } else if failing > 1 {
        format!("{first_detail}; {failing} of {count} fail")
    } else {
        first_detail
    };
    exhaustive_report(output, state, id, cpu, failing == 0, detail.as_bytes());
}

/// Reads one MSR and pairs its value, or zero, with its negative `errno`, or zero.
fn read_msr(runtime: &Runtime<'_>, descriptor: &MsrDescriptor, register: u32) -> (u64, i32) {
    let result = runtime.ops.read_msr(descriptor, register);
    (*result.as_ref().unwrap_or(&0), msr_error(&result))
}

/// Runs exhaustive Hyper-V identity and rate MSR checks.
fn exhaustive_x3(
    runtime: &Runtime<'_>,
    output: &mut dyn Write,
    state: &mut Exhaustive,
    descriptor: &MsrDescriptor,
    cpu: usize,
) {
    let (index, mut error) = read_msr(runtime, descriptor, msr::HV_VP_INDEX);
    let mut detail;
    let mut ok = false;
    let mut tsc = 0;
    let mut lapic = 0;
    if error != 0 || index != cpu as u64 {
        detail = format!(
            "MSR {:#010x} (VP index) is {index} (error {error})",
            msr::HV_VP_INDEX
        );
    } else {
        (tsc, error) = read_msr(runtime, descriptor, msr::HV_TSC_FREQUENCY);
        if error != 0 || tsc == 0 || (state.tsc_hz != 0 && tsc != state.tsc_hz) {
            detail = format!(
                "MSR {:#010x} (TSC rate) is {tsc} (error {error}; first CPU {})",
                msr::HV_TSC_FREQUENCY,
                state.tsc_hz
            );
        } else if state.cpuinfo.known[cpu] && tsc / HZ_PER_KHZ != state.cpuinfo.khz[cpu] {
            detail = format!(
                "floor(F / 1000) is {} kHz but cpu MHz is {} kHz",
                tsc / HZ_PER_KHZ,
                state.cpuinfo.khz[cpu]
            );
        } else {
            (lapic, error) = read_msr(runtime, descriptor, msr::HV_APIC_FREQUENCY);
            if error != 0
                || !LAPIC_RATES_HZ.contains(&lapic)
                || (state.lapic_hz != 0 && lapic != state.lapic_hz)
            {
                detail = format!(
                    "MSR {:#010x} (LAPIC rate) is {lapic} (error {error}; first CPU {})",
                    msr::HV_APIC_FREQUENCY,
                    state.lapic_hz
                );
            } else {
                let (control, control_error) =
                    read_msr(runtime, descriptor, msr::HV_TSC_INVARIANT_CONTROL);
                error = control_error;
                if error != 0 || control != TSC_INVARIANT_ENABLED {
                    detail = format!(
                        "MSR {:#010x} is {control} (error {error})",
                        msr::HV_TSC_INVARIANT_CONTROL
                    );
                } else {
                    detail = String::new();
                    ok = true;
                }
            }
        }
    }
    for register in FAULTING_READS {
        if ok {
            let result = runtime.ops.read_msr(descriptor, register);
            if result != Err(Errno::EIO) {
                ok = false;
                detail = format!(
                    "MSR {register:#010x} read did not fail with EIO (error {})",
                    msr_error(&result)
                );
            }
        }
    }
    if ok {
        detail = format!("vp_index={cpu} tsc_hz={tsc} lapic_hz={lapic} invariant_control=1");
        if state.tsc_hz == 0 {
            state.tsc_hz = tsc;
            state.lapic_hz = lapic;
        }
    }
    exhaustive_report(output, state, "X3", cpu, ok, detail.as_bytes());
}

/// Runs exhaustive invariant-control MSR write/read checks.
fn exhaustive_x4(
    runtime: &Runtime<'_>,
    output: &mut dyn Write,
    state: &mut Exhaustive,
    descriptor: &MsrDescriptor,
    cpu: usize,
) {
    let control = msr::HV_TSC_INVARIANT_CONTROL;
    let mut detail = "0 and 1 accepted, 2 rejected, left at 1".to_owned();
    let mut ok = true;
    for written in 0..=TSC_INVARIANT_ENABLED {
        let error = msr_error(&runtime.ops.write_msr(descriptor, control, written));
        let (value, read_error) = read_msr(runtime, descriptor, control);
        let error = if error != 0 { error } else { read_error };
        if error != 0 || value != written {
            ok = false;
            detail =
                format!("after writing {written}, MSR {control:#010x} is {value} (error {error})");
            break;
        }
    }
    if ok {
        let result = runtime
            .ops
            .write_msr(descriptor, control, TSC_INVARIANT_REJECTED);
        if result != Err(Errno::EIO) {
            ok = false;
            detail = format!(
                "write of {TSC_INVARIANT_REJECTED} to MSR {control:#010x} did not fail with EIO \
                 (error {})",
                msr_error(&result)
            );
        }
    }
    let _ = runtime
        .ops
        .write_msr(descriptor, control, TSC_INVARIANT_ENABLED);
    if ok {
        let (value, error) = read_msr(runtime, descriptor, control);
        if error != 0 || value != TSC_INVARIANT_ENABLED {
            ok = false;
            detail = format!("MSR {control:#010x} is {value} after the check");
        }
    }
    exhaustive_report(output, state, "X4", cpu, ok, detail.as_bytes());
}

/// Verifies that forbidden MSR reads or writes fault with `EIO`.
fn exhaustive_faults(
    runtime: &Runtime<'_>,
    output: &mut dyn Write,
    state: &mut Exhaustive,
    descriptor: &MsrDescriptor,
    cpu: usize,
    writes: bool,
) {
    let list: &[u32] = if writes {
        &FAULTING_WRITES
    } else {
        &FAULTING_ARCHITECTURAL_READS
    };
    let mut detail = format!(
        "{} {} fail with EIO",
        list.len(),
        if writes { "writes" } else { "reads" }
    );
    let mut ok = true;
    for &register in list {
        let error = if writes {
            msr_error(&runtime.ops.write_msr(descriptor, register, 0))
        } else {
            msr_error(&runtime.ops.read_msr(descriptor, register))
        };
        if error != -(Errno::EIO as i32) {
            ok = false;
            detail = format!(
                "{} MSR {register:#x} did not fail with EIO (error {error})",
                if writes { "write of 0 to" } else { "read of" }
            );
            break;
        }
    }
    exhaustive_report(
        output,
        state,
        if writes { "X5" } else { "X6" },
        cpu,
        ok,
        detail.as_bytes(),
    );
}

/// Runs every CI-only exhaustive check on every online CPU, from this thread, which returns to
/// its original affinity.
pub(crate) fn exhaustive(
    runtime: &Runtime<'_>,
    output: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut state = Exhaustive {
        failures: 0,
        tsc_hz: 0,
        lapic_hz: 0,
        cpuinfo: CpuInfo::default(),
    };
    let online = read_text(kernel::ONLINE_CPUS, CPU_LIST_LIMIT)
        .ok()
        .and_then(|text| decoders::parse_cpu_list(&text));
    let Some((cpus, online_count)) = online.filter(|(_, count)| *count > 0) else {
        let _ = writeln!(
            output,
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=fail cpus=0 failures=1"
        );
        return Err(CommandError::Operation);
    };
    if let Ok(text) = read_file(kernel::CPU_INFORMATION) {
        state.cpuinfo = decoders::check_cpuinfo(&text, &cpus, false).0;
    }
    let _affinity = sys::AffinityGuard::save().ok();
    for (cpu, online) in cpus.iter().copied().enumerate() {
        if !online {
            continue;
        }
        if sys::pin_current_thread(cpu).is_err() {
            for id in EXHAUSTIVE_CHECKS {
                exhaustive_report(
                    output,
                    &mut state,
                    id,
                    cpu,
                    false,
                    b"cannot run on this CPU",
                );
            }
            continue;
        }
        let basic = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0);
        let out_of_range = sys::x86::cpuid(basic[0], 0);
        exhaustive_leaves(
            output,
            &mut state,
            "X1",
            cpu,
            EXHAUSTIVE_LEAVES,
            out_of_range,
        );
        exhaustive_leaves(
            output,
            &mut state,
            "X2",
            cpu,
            EXHAUSTIVE_BASES,
            out_of_range,
        );
        match runtime.ops.open_msr(cpu, MsrAccess::ReadWrite) {
            Ok(descriptor) => {
                exhaustive_x3(runtime, output, &mut state, &descriptor, cpu);
                exhaustive_x4(runtime, output, &mut state, &descriptor, cpu);
                exhaustive_faults(runtime, output, &mut state, &descriptor, cpu, true);
                exhaustive_faults(runtime, output, &mut state, &descriptor, cpu, false);
            }
            Err(error) => {
                let detail = format!("open {}: {}", MsrFile::path(cpu), error.desc());
                for id in EXHAUSTIVE_MSR_CHECKS {
                    exhaustive_report(output, &mut state, id, cpu, false, detail.as_bytes());
                }
            }
        }
    }
    let status = if state.failures == 0 { "ok" } else { "fail" };
    let _ = writeln!(
        output,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 status={status} cpus={online_count} failures={}",
        state.failures
    );
    if state.failures == 0 {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_timeout_math_uses_the_abi_constants() {
        assert_eq!(STATUS_WAIT_NS, 30_000_000_000);
        assert_eq!(STATUS_POLL_NS, 10_000_000);
        assert_eq!(TIMER_LIST_WAIT_NS, 200_000_000);
    }

    #[test]
    fn identity_constants_match_the_hyperv_leaves() {
        assert_eq!(HYPERV_MAXIMUM_LEAF, 0x4000_0005);
        assert_eq!(HYPERV_VENDOR, [0x7263_694d, 0x666f_736f, 0x7648_2074]);
        assert_eq!(HYPERV_INTERFACE, 0x3123_7648);
        assert_eq!(HYPERV_BUILD, [0x0058_564e, 0x0001_0000]);
        assert_eq!(HYPERV_PRIVILEGES, 0x8860);
        assert_eq!(HYPERV_FEATURES, 0x100);
    }

    #[test]
    fn feature_bits_are_labeled_as_the_c_program_labels_them() {
        let labels = LEAF1_TIME_BITS
            .iter()
            .chain([&TSC_ADJUST_BIT, &RDTSCP_BIT])
            .map(FeatureBit::label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "CPUID.1:ECX[31] (hypervisor)",
                "CPUID.1:ECX[24] (TSC-deadline)",
                "CPUID.1:ECX[15] (PDCM)",
                "CPUID.1:EDX[4] (TSC)",
                "CPUID.7.0:EBX[1] (TSC_ADJUST)",
                "CPUID.80000001:EDX[27] (RDTSCP)",
            ]
        );
    }

    #[test]
    fn msr_diagnostics_name_the_registers_in_hexadecimal() {
        assert_eq!(format!("{:#010x}", msr::HV_VP_INDEX), "0x40000002");
        assert_eq!(format!("{:#x}", msr::IA32_TSC_DEADLINE), "0x6e0");
        assert_eq!(MsrFile::path(3), "/dev/cpu/3/msr");
    }
}
