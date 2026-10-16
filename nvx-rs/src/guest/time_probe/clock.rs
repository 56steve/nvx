//! Clock, CPU, hypervisor, kernel-log, timer, and optional MSR diagnostics.

use std::ffi::CStr;
use std::io::Write;
use std::time::Duration;

use super::format::{f0, f9, utc_seconds};
use super::support::{
    NANOSECONDS_PER_SECOND, bad_option, cpuinfo_has_flag, current_clocksource, detect_tsc_rate,
    kmsg_foreach, measure_tsc_hz, option_value, parse_rate, parse_u64, read_file, read_first_line,
    read_or_empty, round_ll,
};
use crate::guest::status::CommandError;
use crate::guest::sys::x86::{cpuid, msr};
use crate::guest::sys::{self, Errno, MsrAccess, MsrFile, kernel};

/// Milliseconds over which the clock probe measures the TSC rate without `--measure-ms`.
const DEFAULT_MEASURE_MS: u64 = 200;
/// The most milliseconds that `--measure-ms` accepts.
const MEASURE_LIMIT_MS: u64 = 60_000;
/// Bytes of the probe's buffer for a line of a kernel file, its NUL included.
const LINE_LIMIT: usize = 4096;
/// The last hypervisor CPUID leaf that the probe prints.
const LAST_PRINTED_HYPERVISOR_LEAF: u32 = 0x4000_0010;
/// Hyper-V's vendor signature in CPUID leaf 0x40000000.
const HYPERV_VENDOR: &[u8] = b"Microsoft Hv";
/// Longest list of devices, modes, or flags that a summary prints, as the C buffers held.
const LIST_LIMIT: usize = 256;
/// Longest list of tick modes that a summary prints.
const MODE_LIST_LIMIT: usize = 64;
/// Longest list of clock-event states that a summary prints.
const STATE_LIST_LIMIT: usize = 128;
/// Longest list of CPU rates that the cpuinfo summary prints.
const RATE_LIST_LIMIT: usize = 512;
/// Longest timer-list line that the probe reads at once, as C's `strtok_r` buffer held.
const TIMER_LINE_LIMIT: usize = 511;
/// Longest clock-event device or handler name that the probe keeps.
const NAME_LIMIT: usize = 63;
/// The MSRs that `--msr` prints, with their architectural names.
const PRINTED_MSRS: [(u32, &str); 7] = [
    (msr::IA32_TSC, "IA32_TSC"),
    (msr::IA32_TSC_ADJUST, "IA32_TSC_ADJUST"),
    (msr::HV_VP_INDEX, "HV_X64_MSR_VP_INDEX"),
    (msr::HV_TSC_FREQUENCY, "HV_X64_MSR_TSC_FREQUENCY"),
    (msr::HV_APIC_FREQUENCY, "HV_X64_MSR_APIC_FREQUENCY"),
    (
        msr::HV_TSC_INVARIANT_CONTROL,
        "HV_X64_MSR_TSC_INVARIANT_CONTROL",
    ),
    (msr::KVM_SYSTEM_TIME_NEW, "MSR_KVM_SYSTEM_TIME_NEW"),
];

/// Returns bit `bit` of `register` as the 0 or 1 that the probe prints.
fn bit(register: u32, bit: u32) -> u8 {
    u8::from(cpuid::bit(register, bit))
}

/// CPU flags whose presence is summarized in probe output.
const TSC_FLAGS: &[&str] = &[
    "tsc",
    "rdtscp",
    "constant_tsc",
    "nonstop_tsc",
    "nonstop_tsc_s3",
    "tsc_known_freq",
    "tsc_reliable",
    "tsc_deadline_timer",
    "tsc_adjust",
    "art",
    "hypervisor",
    "arat",
    "aperfmperf",
    "hwp",
    "x2apic",
];
/// Kernel-message substrings identifying the active hypervisor.
const HYPERVISOR_PATTERNS: &[&[u8]] = &[
    b"Hypervisor detected",
    b"Hyper-V",
    b"Booting paravirtualized kernel",
    b"kvm-guest",
];
/// Kernel-message substrings relevant to clocks and stalls.
const TIME_PATTERNS: &[&[u8]] = &[
    b"tsc",
    b"TSC",
    b"clocksource",
    b"clockevent",
    b"kvm-clock",
    b"APIC timer",
    b"LAPIC",
    b"lapic",
    b"calibrat",
    b"Calibrat",
    b"unchecked MSR",
    b"hpet",
    b"HPET",
    b"timekeeping",
    b"sched_clock",
    b"detected stall",
    b"watchdog",
    b"lockup",
    b"hung_task",
    b"jiffies",
    b"rtc",
    b"RTC",
];
/// Kernel time/watchdog controls printed by the clock probe.
const PARAM_FILES: [&CStr; 7] = [
    kernel::RCU_STALL_SUPPRESS,
    kernel::RCU_STALL_TIMEOUT,
    kernel::WATCHDOG,
    kernel::SOFT_WATCHDOG,
    kernel::NMI_WATCHDOG,
    kernel::WATCHDOG_THRESHOLD,
    kernel::HUNG_TASK_TIMEOUT,
];

/// Returns whether one byte substring occurs in another.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Appends one comma-separated value once within a byte limit.
fn add_unique(list: &mut String, item: &str, limit: usize) {
    if list.split(',').any(|existing| existing == item) {
        return;
    }
    if !list.is_empty() && list.len() < limit.saturating_sub(1) {
        list.push(',');
    }
    let available = limit.saturating_sub(1).saturating_sub(list.len());
    list.extend(item.chars().take(available));
}

/// Replaces bytes unsafe for a one-token marker with underscores.
fn sanitize_token(text: &mut [u8]) {
    for byte in text {
        if !(byte.is_ascii_alphanumeric() || matches!(*byte, b'#' | b'-' | b'.')) {
            *byte = b'_';
        }
    }
}

/// Reads and prints one named clock, including UTC for realtime.
fn print_clock(output: &mut dyn Write, name: &str, clock: sys::Clock) -> Option<sys::ClockTime> {
    let time = match sys::clock_time(clock) {
        Ok(time) => time,
        Err(error) => {
            let _ = writeln!(output, "{name:<20} unavailable ({})", error.desc());
            return None;
        }
    };
    let _ = write!(
        output,
        "{name:<20} {}.{:09}",
        time.seconds, time.nanoseconds
    );
    if clock == sys::Clock::Realtime {
        let _ = write!(
            output,
            "  ({}.{:09}Z)",
            utc_seconds(time.seconds),
            time.nanoseconds
        );
    }
    let _ = writeln!(output);
    Some(time)
}

#[derive(Default)]
/// Counters accumulated while filtering kernel messages.
struct Filter {
    /// Number of messages printed for the requested patterns.
    matches: i32,
    /// Number of TSC-instability messages.
    tsc_unstable: i32,
    /// Number of unchecked-MSR messages.
    unchecked_msr: i32,
}

/// Prints matching kernel messages and updates diagnostic counters.
fn print_kmsg(
    output: &mut dyn Write,
    patterns: &[&[u8]],
    filter: &mut Filter,
) -> Result<(), Errno> {
    kmsg_foreach(|usec, text| {
        if contains(text, b"Marking TSC unstable") || contains(text, b"TSC unstable") {
            filter.tsc_unstable += 1;
        }
        if contains(text, b"unchecked MSR access") {
            filter.unchecked_msr += 1;
        }
        if patterns.iter().any(|pattern| contains(text, pattern)) {
            let _ = write!(output, "[{:5}.{:06}] ", usec / 1_000_000, usec % 1_000_000);
            let _ = output.write_all(text);
            let _ = output.write_all(b"\n");
            filter.matches += 1;
        }
    })
}

#[derive(Clone)]
/// Parsed `/proc/timer_list` data for one clock-event section.
struct TickInfo {
    /// Whether the section was present.
    seen: bool,
    /// Kernel tick mode.
    tick_mode: i32,
    /// Clock-event state.
    state: i32,
    /// High-resolution-timer active flag.
    hres_active: i32,
    /// Tickless mode flag.
    nohz: i32,
    /// Clock-event device name.
    device: String,
    /// Event-handler name.
    handler: String,
}

impl Default for TickInfo {
    fn default() -> Self {
        Self {
            seen: false,
            tick_mode: -1,
            state: -1,
            hres_active: -1,
            nohz: -1,
            device: String::new(),
            handler: String::new(),
        }
    }
}

/// Scans one signed decimal value following a literal prefix.
fn scan_i32(line: &[u8], prefix: &[u8]) -> Option<i32> {
    let value = line.strip_prefix(prefix)?;
    let value = value
        .iter()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    (!value.is_empty())
        .then(|| std::str::from_utf8(&value).ok()?.parse().ok())
        .flatten()
}

/// Scans one whitespace-delimited word following a literal prefix.
fn scan_word(line: &[u8], prefix: &[u8]) -> Option<String> {
    let value = line.strip_prefix(prefix)?;
    let value = value
        .split(u8::is_ascii_whitespace)
        .next()
        .unwrap_or_default();
    (!value.is_empty())
        .then(|| String::from_utf8_lossy(&value[..value.len().min(NAME_LIMIT)]).into_owned())
}

/// Selects timer-list lines using C `strtok_r`-compatible newline handling.
fn timer_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let newline = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|length| start + length);
        let end = newline.unwrap_or(bytes.len());
        let mut chunk = start;
        while chunk < end {
            let next = (chunk + TIMER_LINE_LIMIT).min(end);
            lines.push(&bytes[chunk..next]);
            chunk = next;
        }
        if chunk == start {
            lines.push(&[]);
        }
        start = newline.map_or(bytes.len(), |newline| newline + 1);
    }
    lines
}

/// Returns the stable display name for a kernel tick mode.
fn tick_mode_name(mode: i32) -> &'static str {
    match mode {
        0 => "periodic",
        1 => "oneshot",
        _ => "unknown",
    }
}

/// Returns the stable display name for a clock-event state.
fn state_name(state: i32) -> &'static str {
    [
        "detached",
        "shutdown",
        "periodic",
        "oneshot",
        "oneshot_stopped",
    ]
    .get(state as usize)
    .copied()
    .unwrap_or("unknown")
}

/// Formats C `%#010x`, including its alternate prefix and width.
fn alternate_hex_width_10(value: u32) -> String {
    if value == 0 {
        "0000000000".to_owned()
    } else {
        format!("{value:#010x}")
    }
}

/// Prints timer-list sections and returns the one-line marker summary.
fn print_timer_list(output: &mut dyn Write) -> String {
    let text = match read_file(kernel::TIMER_LIST) {
        Ok(text) => text,
        Err(error) => {
            let _ = writeln!(output, "timer_list: unavailable ({})", error.desc());
            return "clockevents=unavailable".to_owned();
        }
    };
    let mut cpus = vec![TickInfo::default(); super::MAX_CPUS];
    let mut broadcast = TickInfo::default();
    let mut current = None;
    let mut high_resolution_cpu = None;
    let mut tick_mode = -1;
    for line in timer_lines(&text) {
        if let Some(value) = scan_i32(line, b"cpu: ") {
            high_resolution_cpu = usize::try_from(value)
                .ok()
                .filter(|cpu| *cpu < super::MAX_CPUS);
            current = None;
        } else if line.starts_with(b"Tick Device: mode:") {
            tick_mode = scan_i32(line, b"Tick Device: mode:").unwrap_or(0);
            high_resolution_cpu = None;
            current = None;
        } else if line.starts_with(b"Broadcast device") {
            broadcast.seen = true;
            broadcast.tick_mode = tick_mode;
            current = Some(super::MAX_CPUS);
        } else if let Some(value) = scan_i32(line, b"Per CPU device: ") {
            current = usize::try_from(value)
                .ok()
                .filter(|cpu| *cpu < super::MAX_CPUS);
            if let Some(cpu) = current {
                cpus[cpu].seen = true;
                cpus[cpu].tick_mode = tick_mode;
            }
        } else if let Some(section) = current {
            let target = if section == super::MAX_CPUS {
                &mut broadcast
            } else {
                &mut cpus[section]
            };
            if let Some(value) = scan_word(line, b"Clock Event Device: ") {
                target.device = value;
            } else if let Some(value) = scan_i32(line, b" mode: ") {
                target.state = value;
            } else if let Some(value) = scan_word(line, b" event_handler: ") {
                target.handler = value;
            }
        } else if let Some(cpu) = high_resolution_cpu {
            if let Some(value) = scan_i32(line, b" .hres_active : ") {
                cpus[cpu].hres_active = value;
            } else if let Some(value) =
                scan_i32(line, b" .nohz_mode : ").or_else(|| scan_i32(line, b" .nohz : "))
            {
                cpus[cpu].nohz = value;
            }
        }
    }
    if broadcast.seen {
        let _ = writeln!(
            output,
            "broadcast: device={} state={} tick_mode={} handler={}",
            if broadcast.device.is_empty() {
                "none"
            } else {
                &broadcast.device
            },
            state_name(broadcast.state),
            tick_mode_name(broadcast.tick_mode),
            if broadcast.handler.is_empty() {
                "none"
            } else {
                &broadcast.handler
            }
        );
    }
    let mut count = 0;
    let mut devices = String::new();
    let mut modes = String::new();
    let mut states = String::new();
    for (cpu, info) in cpus.iter().enumerate().filter(|(_, info)| info.seen) {
        count += 1;
        let device = if info.device.is_empty() {
            "none"
        } else {
            &info.device
        };
        let handler = if info.handler.is_empty() {
            "none"
        } else {
            &info.handler
        };
        let _ = writeln!(
            output,
            "cpu{cpu}: device={device} state={} tick_mode={} handler={handler} \
             hres_active={} nohz={}",
            state_name(info.state),
            tick_mode_name(info.tick_mode),
            info.hres_active,
            info.nohz
        );
        add_unique(&mut devices, device, LIST_LIMIT);
        add_unique(&mut modes, tick_mode_name(info.tick_mode), MODE_LIST_LIMIT);
        add_unique(&mut states, state_name(info.state), STATE_LIST_LIMIT);
    }
    if !broadcast.seen && count == 0 {
        let _ = writeln!(
            output,
            "timer_list: no tick devices found (file empty or masked)"
        );
    }
    format!(
        "clockevent_cpus={count} clockevent_devices={} tick_modes={} clockevent_states={} \
         broadcast={}:{}",
        if devices.is_empty() { "none" } else { &devices },
        if modes.is_empty() { "none" } else { &modes },
        if states.is_empty() { "none" } else { &states },
        if broadcast.seen && !broadcast.device.is_empty() {
            &broadcast.device
        } else {
            "none"
        },
        if broadcast.seen {
            state_name(broadcast.state)
        } else {
            "none"
        }
    )
}

#[derive(Default)]
/// CPUID values reused by the final clock-probe marker.
struct CpuidSummary {
    /// Hypervisor vendor bytes.
    vendor: Vec<u8>,
    /// Maximum Hyper-V leaf.
    max_leaf: u32,
    /// Hyper-V feature EAX.
    features_eax: u32,
    /// Hyper-V feature EDX.
    features_edx: u32,
    /// Whether CPUID reports a hypervisor.
    hypervisor: bool,
}

/// Prints architectural and hypervisor CPUID leaves.
fn print_cpuid(output: &mut dyn Write) -> CpuidSummary {
    let basic = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0)[0];
    let extended = sys::x86::cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0)[0];
    let leaf1 = sys::x86::cpuid(cpuid::LEAF_FEATURES, 0);
    let signature = cpuid::Signature::decode(leaf1[0]);
    let mut summary = CpuidSummary {
        hypervisor: cpuid::bit(leaf1[2], cpuid::FEATURES_ECX_HYPERVISOR),
        ..CpuidSummary::default()
    };
    let _ = writeln!(
        output,
        "cpuid.1: family={} model={} stepping={} edx.tsc={} ecx.tsc_deadline={} \
         ecx.hypervisor={}",
        signature.family,
        signature.model,
        signature.stepping,
        bit(leaf1[3], cpuid::FEATURES_EDX_TSC),
        bit(leaf1[2], cpuid::FEATURES_ECX_TSC_DEADLINE),
        u8::from(summary.hypervisor)
    );
    if basic >= cpuid::LEAF_POWER_MANAGEMENT {
        let value = sys::x86::cpuid(cpuid::LEAF_POWER_MANAGEMENT, 0);
        let _ = writeln!(
            output,
            "cpuid.6: eax.arat={} eax.hwp={} ecx.aperfmperf={}",
            bit(value[0], cpuid::POWER_MANAGEMENT_EAX_ARAT),
            bit(value[0], cpuid::POWER_MANAGEMENT_EAX_HWP),
            bit(value[2], cpuid::POWER_MANAGEMENT_ECX_APERFMPERF)
        );
    }
    if basic >= cpuid::LEAF_STRUCTURED_FEATURES {
        let value = sys::x86::cpuid(cpuid::LEAF_STRUCTURED_FEATURES, 0);
        let _ = writeln!(
            output,
            "cpuid.7.0: ebx.tsc_adjust={}",
            bit(value[1], cpuid::STRUCTURED_FEATURES_EBX_TSC_ADJUST)
        );
    }
    if basic >= cpuid::LEAF_PERFORMANCE_MONITORING {
        let value = sys::x86::cpuid(cpuid::LEAF_PERFORMANCE_MONITORING, 0);
        let _ = writeln!(
            output,
            "cpuid.0xa: eax={} (pmu_version={})",
            alternate_hex_width_10(value[0]),
            value[0] & cpuid::PERFORMANCE_MONITORING_VERSION_MASK
        );
    }
    if basic >= cpuid::LEAF_TSC_CRYSTAL {
        let value = sys::x86::cpuid(cpuid::LEAF_TSC_CRYSTAL, 0);
        let _ = write!(
            output,
            "cpuid.0x15: eax.denominator={} ebx.numerator={} ecx.crystal_hz={}",
            value[0], value[1], value[2]
        );
        if value[0] != 0 && value[1] != 0 && value[2] != 0 {
            let _ = write!(
                output,
                " (tsc_hz={})",
                f0(value[2] as f64 * value[1] as f64 / value[0] as f64)
            );
        }
        let _ = writeln!(output);
    }
    if basic >= cpuid::LEAF_FREQUENCY {
        let value = sys::x86::cpuid(cpuid::LEAF_FREQUENCY, 0);
        let _ = writeln!(
            output,
            "cpuid.0x16: eax.base_mhz={} ebx.max_mhz={} ecx.bus_mhz={}",
            value[0] & cpuid::FREQUENCY_MHZ_MASK,
            value[1] & cpuid::FREQUENCY_MHZ_MASK,
            value[2] & cpuid::FREQUENCY_MHZ_MASK
        );
    }
    if extended >= cpuid::LEAF_EXTENDED_FEATURES {
        let value = sys::x86::cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0);
        let _ = writeln!(
            output,
            "cpuid.0x80000001: edx.rdtscp={}",
            bit(value[3], cpuid::EXTENDED_FEATURES_EDX_RDTSCP)
        );
    }
    if extended >= cpuid::LEAF_ADVANCED_POWER {
        let value = sys::x86::cpuid(cpuid::LEAF_ADVANCED_POWER, 0);
        let _ = writeln!(
            output,
            "cpuid.0x80000007: edx.invariant_tsc={}",
            bit(value[3], cpuid::ADVANCED_POWER_EDX_INVARIANT_TSC)
        );
    }
    if !summary.hypervisor {
        let _ = writeln!(
            output,
            "cpuid.0x40000000: not queried (hypervisor bit clear)"
        );
        summary.vendor = b"none".to_vec();
        return summary;
    }
    let value = sys::x86::cpuid(cpuid::LEAF_HYPERVISOR, 0);
    summary.max_leaf = value[0];
    for register in &value[1..] {
        summary.vendor.extend_from_slice(&register.to_ne_bytes());
    }
    for byte in &mut summary.vendor {
        if !(byte.is_ascii_graphic() || *byte == b' ') {
            *byte = b'.';
        }
    }
    let _ = writeln!(
        output,
        "cpuid.0x40000000: eax.max_leaf={:#x} vendor=\"{}\"",
        summary.max_leaf,
        String::from_utf8_lossy(&summary.vendor)
    );
    let last = summary.max_leaf.min(LAST_PRINTED_HYPERVISOR_LEAF);
    for leaf in cpuid::LEAF_HYPERVISOR + 1..=last {
        let value = sys::x86::cpuid(leaf, 0);
        let _ = writeln!(
            output,
            "cpuid.{leaf:#x}: eax={} ebx={} ecx={} edx={}",
            alternate_hex_width_10(value[0]),
            alternate_hex_width_10(value[1]),
            alternate_hex_width_10(value[2]),
            alternate_hex_width_10(value[3])
        );
        if leaf == cpuid::LEAF_HYPERV_FEATURES {
            summary.features_eax = value[0];
            summary.features_edx = value[3];
        }
    }
    if summary.vendor == HYPERV_VENDOR && summary.max_leaf >= cpuid::LEAF_HYPERV_FEATURES {
        let _ = writeln!(
            output,
            "hyperv.features: hypercall={} vp_index={} access_frequency_msrs={} \
             access_tsc_invariant={} frequency_msrs_available={}",
            bit(summary.features_eax, cpuid::HYPERV_EAX_HYPERCALL),
            bit(summary.features_eax, cpuid::HYPERV_EAX_VP_INDEX),
            bit(summary.features_eax, cpuid::HYPERV_EAX_ACCESS_FREQUENCY),
            bit(summary.features_eax, cpuid::HYPERV_EAX_ACCESS_TSC_INVARIANT),
            bit(summary.features_edx, cpuid::HYPERV_EDX_FREQUENCY_MSRS)
        );
    }
    summary
}

/// Prints optional CPU-zero MSRs, reporting `EIO` as a trapped `#GP`.
fn print_msrs(output: &mut dyn Write) {
    let device = match MsrFile::open(0, MsrAccess::Read) {
        Ok(device) => device,
        Err(error) => {
            let _ = writeln!(
                output,
                "msr: {} unavailable ({})",
                MsrFile::path(0),
                error.desc()
            );
            return;
        }
    };
    for (index, name) in PRINTED_MSRS {
        match device.read(index) {
            Ok(value) => {
                let _ = writeln!(
                    output,
                    "msr {index:#010x} {name:<34} = {value:#x} ({value})"
                );
            }
            Err(error) => {
                let reason = if error == Errno::EIO {
                    "read faulted (#GP)"
                } else {
                    error.desc()
                };
                let _ = writeln!(output, "msr {index:#010x} {name:<34} : {reason}");
            }
        }
    }
}

/// Prints processor count, model, and observed MHz values from cpuinfo.
fn cpuinfo_summary(output: &mut dyn Write) {
    let text = read_or_empty(kernel::CPU_INFORMATION);
    let mut processors = 0;
    let mut model_printed = false;
    let mut mhz = String::new();
    for line in text.split(|byte| *byte == b'\n') {
        let colon = line.iter().position(|byte| *byte == b':');
        if line.starts_with(b"processor") {
            processors += 1;
        } else if !model_printed && line.starts_with(b"model name") {
            if let Some(colon) = colon {
                let _ = write!(output, "model name:");
                let _ = output.write_all(&line[colon + 1..]);
                let _ = writeln!(output);
                model_printed = true;
            }
        } else if line.starts_with(b"cpu MHz")
            && let Some(colon) = colon
        {
            let start = (colon + 2).min(line.len());
            add_unique(
                &mut mhz,
                &String::from_utf8_lossy(&line[start..]),
                RATE_LIST_LIMIT,
            );
        }
    }
    let _ = writeln!(
        output,
        "processors: {processors}\ncpu MHz: {}",
        if mhz.is_empty() { "unknown" } else { &mhz }
    );
}

/// Parses options and prints the complete clock diagnostic report.
pub(super) fn run(
    arguments: &[&[u8]],
    output: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut measure_ms = DEFAULT_MEASURE_MS;
    let mut override_hz = 0.0;
    let mut read_msrs = false;
    let mut index = 0;
    while index < arguments.len() {
        if let Some(value) = option_value(arguments, &mut index, b"--measure-ms") {
            measure_ms = parse_u64(value)
                .filter(|value| *value <= MEASURE_LIMIT_MS)
                .ok_or_else(|| bad_option(b"clock", arguments[index], err))?;
        } else if let Some(value) = option_value(arguments, &mut index, b"--tsc-hz") {
            override_hz =
                parse_rate(value).ok_or_else(|| bad_option(b"clock", arguments[index], err))?;
        } else if arguments[index] == b"--msr" {
            read_msrs = true;
        } else {
            return Err(bad_option(b"clock", arguments[index], err));
        }
        index += 1;
    }
    let rate = detect_tsc_rate(override_hz);
    let _ = writeln!(output, "nvx-time-probe {} clock\n== clocks", super::VERSION);
    // The C probe printed -1 for a CPU that `sched_getcpu` could not report.
    let cpu = sys::current_cpu().map_or(-1, |cpu| cpu as i64);
    let tsc = sys::x86::rdtsc_fenced();
    print_clock(output, "CLOCK_REALTIME", sys::Clock::Realtime);
    let monotonic =
        print_clock(output, "CLOCK_MONOTONIC", sys::Clock::Monotonic).unwrap_or_default();
    print_clock(output, "CLOCK_MONOTONIC_RAW", sys::Clock::MonotonicRaw);
    print_clock(output, "CLOCK_BOOTTIME", sys::Clock::Boottime);
    let _ = write!(output, "{:<20} {tsc} on cpu {cpu}", "TSC");
    if rate.hz > 0.0 {
        let seconds = tsc as f64 / rate.hz;
        let difference = tsc as f64 * NANOSECONDS_PER_SECOND / rate.hz
            - (monotonic.seconds as f64 * NANOSECONDS_PER_SECOND + monotonic.nanoseconds as f64);
        let _ = write!(
            output,
            " ({} s; tsc_minus_monotonic_ns={})",
            f9(seconds),
            round_ll(difference)
        );
    }
    let _ = writeln!(
        output,
        "\ntsc_hz={} tsc_hz_source={}",
        f0(rate.hz),
        rate.source
    );
    if measure_ms > 0 {
        let _ = writeln!(
            output,
            "tsc_hz_measured={} (over {measure_ms} ms against CLOCK_MONOTONIC_RAW)",
            f0(measure_tsc_hz(Duration::from_millis(measure_ms)))
        );
    }

    let _ = writeln!(output, "== clocksource");
    let clocksource = current_clocksource();
    let mut available = String::new();
    if let Ok(line) = read_first_line(kernel::AVAILABLE_CLOCKSOURCES, LINE_LIMIT) {
        for token in line
            .split(|byte| *byte == b' ')
            .filter(|token| !token.is_empty())
        {
            add_unique(&mut available, &String::from_utf8_lossy(token), LIST_LIMIT);
        }
    }
    let _ = writeln!(
        output,
        "current: {}\navailable: {}",
        String::from_utf8_lossy(&clocksource),
        if available.is_empty() {
            "unknown"
        } else {
            &available
        }
    );

    let _ = writeln!(output, "== cpuinfo");
    cpuinfo_summary(output);
    let mut present = String::new();
    let mut absent = String::new();
    for flag in TSC_FLAGS {
        if cpuinfo_has_flag(flag.as_bytes()) {
            add_unique(&mut present, flag, LIST_LIMIT);
        } else {
            add_unique(&mut absent, flag, LIST_LIMIT);
        }
    }
    let _ = writeln!(
        output,
        "time flags present: {}\ntime flags absent: {}",
        if present.is_empty() { "none" } else { &present },
        if absent.is_empty() { "none" } else { &absent }
    );
    let _ = writeln!(output, "== cpuid");
    let mut ids = print_cpuid(output);
    if read_msrs {
        let _ = writeln!(output, "== msr (cpu 0)");
        print_msrs(output);
    }

    let _ = writeln!(output, "== kernel");
    if let Ok(line) = read_first_line(kernel::COMMAND_LINE, LINE_LIMIT) {
        let _ = write!(output, "cmdline: ");
        let _ = output.write_all(&line);
        let _ = writeln!(output);
    }
    if let Ok(line) = read_first_line(kernel::ONLINE_CPUS, LINE_LIMIT) {
        let _ = write!(output, "online cpus: ");
        let _ = output.write_all(&line);
        let _ = writeln!(output);
    }
    for path in PARAM_FILES {
        let name = path.to_string_lossy();
        match read_first_line(path, LINE_LIMIT) {
            Ok(line) => {
                let _ = write!(output, "{name} = ");
                let _ = output.write_all(&line);
                let _ = writeln!(output);
            }
            Err(_) => {
                let _ = writeln!(output, "{name}: absent");
            }
        }
    }

    let _ = writeln!(output, "== kmsg: hypervisor");
    let mut hypervisor = Filter::default();
    match print_kmsg(output, HYPERVISOR_PATTERNS, &mut hypervisor) {
        Err(error) => {
            let _ = writeln!(output, "(kernel log unavailable: {})", error.desc());
        }
        Ok(()) if hypervisor.matches == 0 => {
            let _ = writeln!(output, "(none)");
        }
        Ok(()) => {}
    }
    let _ = writeln!(output, "== kmsg: time");
    let mut time = Filter::default();
    match print_kmsg(output, TIME_PATTERNS, &mut time) {
        Err(error) => {
            let _ = writeln!(output, "(kernel log unavailable: {})", error.desc());
        }
        Ok(()) if time.matches == 0 => {
            let _ = writeln!(output, "(none)");
        }
        Ok(()) => {}
    }
    let _ = writeln!(output, "== timer_list");
    let timer_summary = print_timer_list(output);
    sanitize_token(&mut ids.vendor);
    let _ = writeln!(
        output,
        "NVX-TIME-PROBE clock clocksource={} available={} tsc_hz={} tsc_hz_source={} \
         hv_vendor={} hv_max_leaf={:#x} hv_features_eax={:#x} hv_features_edx={:#x} \
         flags={} missing_flags={} tsc_unstable_msgs={} unchecked_msr_msgs={} {}",
        String::from_utf8_lossy(&clocksource),
        if available.is_empty() {
            "unknown"
        } else {
            &available
        },
        f0(rate.hz),
        rate.source,
        String::from_utf8_lossy(&ids.vendor),
        ids.max_leaf,
        ids.features_eax,
        ids.features_edx,
        if present.is_empty() { "none" } else { &present },
        if absent.is_empty() { "none" } else { &absent },
        time.tsc_unstable,
        time.unchecked_msr,
        timer_summary
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_sanitizing_keeps_only_marker_safe_bytes() {
        let mut value = b"Microsoft Hv/x".to_vec();
        sanitize_token(&mut value);
        assert_eq!(value, b"Microsoft_Hv_x");
    }

    #[test]
    fn unique_lists_do_not_match_substrings() {
        let mut list = String::new();
        add_unique(&mut list, "tsc", 256);
        add_unique(&mut list, "constant_tsc", 256);
        add_unique(&mut list, "tsc", 256);
        assert_eq!(list, "tsc,constant_tsc");
    }
}
