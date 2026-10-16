//! Parsing, affinity, kernel-log access, and TSC-rate discovery.

use std::ffi::CStr;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

use crate::guest::parse::{parse_decimal, parse_f64};
use crate::guest::status::CommandError;
use crate::guest::sys::{self, Errno, Mode, OpenFlags, kernel};

use super::MAX_CPUS;

/// Maximum structured `/dev/kmsg` record read by the probe.
const KMSG_RECORD_MAX: usize = 8192;
/// Size of the buffer for the kernel's log when the kernel does not report the log's size.
const FALLBACK_LOG_SIZE: usize = 1 << 20;
/// Longest CPU list that the probe prints, as its C buffer held.
const CPU_LIST_PRINT_MAX: usize = 255;
/// Bytes of the probe's buffers for a one-line sysfs or procfs value, its NUL included.
pub(super) const VALUE_LIMIT: usize = 64;
/// Microseconds in a second, which scale a syslog timestamp's seconds.
const MICROSECONDS_PER_SECOND: f64 = 1e6;
/// Nanoseconds in a second, which scale a TSC rate's cycles.
pub(super) const NANOSECONDS_PER_SECOND: f64 = 1e9;
/// kHz in a MHz, and Hz in a kHz.
const THOUSAND: f64 = 1000.0;
/// The interval over which the probe measures the TSC rate when nothing reports it.
const MEASURE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
/// Detected TSC frequency and the source that supplied it.
pub(super) struct TscRate {
    /// Cycles per second.
    pub(super) hz: f64,
    /// Stable source token printed in output.
    pub(super) source: &'static str,
}

/// Parses an integer option: plain decimal, or hexadecimal after `0x` or `0X`.
pub(super) fn parse_u64(text: &[u8]) -> Option<u64> {
    std::ffi::CString::new(text)
        .ok()
        .and_then(|text| crate::guest::parse::parse_u64(&text))
}

/// Parses a positive, finite rate option, such as `--tsc-hz`, in decimal floating point.
pub(super) fn parse_rate(text: &[u8]) -> Option<f64> {
    parse_f64(text).filter(|value| *value > 0.0)
}

/// Extracts `--name=value` or `--name value` and advances `index`.
pub(super) fn option_value<'a>(
    arguments: &'a [&'a [u8]],
    index: &mut usize,
    name: &[u8],
) -> Option<&'a [u8]> {
    let argument = arguments[*index];
    if !argument.starts_with(name) {
        return None;
    }
    if argument.get(name.len()) == Some(&b'=') {
        return Some(&argument[name.len() + 1..]);
    }
    if argument.len() == name.len() && *index + 1 < arguments.len() {
        *index += 1;
        return Some(arguments[*index]);
    }
    None
}

/// Prints the byte-exact invalid-option diagnostic and fails with the usage status.
pub(super) fn bad_option(command: &[u8], argument: &[u8], err: &mut dyn Write) -> CommandError {
    sys::report(
        err,
        &[
            b"nvx-time-probe ",
            command,
            b": invalid option or value: ",
            argument,
            b"\n",
        ],
    );
    CommandError::Usage
}

/// Returns CPUs in the calling thread's affinity mask.
pub(super) fn affinity_cpus() -> Result<Vec<usize>, Errno> {
    sys::current_affinity().map(|set| {
        let mut cpus = set.cpus();
        cpus.truncate(MAX_CPUS);
        cpus
    })
}

/// Verifies every requested CPU belongs to the affinity mask.
pub(super) fn check_cpus_allowed(command: &[u8], cpus: &[usize], err: &mut dyn Write) -> bool {
    let allowed = match sys::current_affinity() {
        Ok(allowed) => allowed,
        Err(error) => {
            sys::report(
                err,
                &[
                    b"nvx-time-probe ",
                    command,
                    b": sched_getaffinity: ",
                    error.desc().as_bytes(),
                    b"\n",
                ],
            );
            return false;
        }
    };
    for &cpu in cpus {
        if !allowed.contains(cpu) {
            sys::report(
                err,
                &[
                    b"nvx-time-probe ",
                    command,
                    format!(": cpu {cpu} is not in the affinity mask (offline or excluded)\n")
                        .as_bytes(),
                ],
            );
            return false;
        }
    }
    true
}

/// Parses one CPU number of a CPU list, below `MAX_CPUS`.
fn parse_cpu(text: &[u8]) -> Option<usize> {
    parse_decimal(text)
        .and_then(|cpu| usize::try_from(cpu).ok())
        .filter(|cpu| *cpu < MAX_CPUS)
}

/// Parses a comma-separated list of CPUs and CPU ranges, such as `0-3,6`, in plain decimal,
/// that names each CPU at most once.
pub(super) fn parse_cpu_list(text: &[u8]) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    if text.is_empty() {
        return Some(cpus);
    }
    for item in text.split(|byte| *byte == b',') {
        let (first, last) = match item.iter().position(|byte| *byte == b'-') {
            Some(dash) => (parse_cpu(&item[..dash])?, parse_cpu(&item[dash + 1..])?),
            None => {
                let cpu = parse_cpu(item)?;
                (cpu, cpu)
            }
        };
        if last < first {
            return None;
        }
        for cpu in first..=last {
            if cpus.contains(&cpu) {
                return None;
            }
            cpus.push(cpu);
        }
    }
    Some(cpus)
}

/// Compresses sorted CPU numbers into comma/range syntax.
pub(super) fn format_cpu_list(cpus: &[usize]) -> String {
    let mut output = String::new();
    let mut index = 0;
    while index < cpus.len() {
        let first = cpus[index];
        let mut last = first;
        while index + 1 < cpus.len() && cpus[index + 1] == last + 1 {
            index += 1;
            last = cpus[index];
        }
        if !output.is_empty() {
            output.push(',');
        }
        output.push_str(&first.to_string());
        if last != first {
            output.push('-');
            output.push_str(&last.to_string());
        }
        index += 1;
    }
    output.truncate(CPU_LIST_PRINT_MAX);
    output
}

/// Reads the whole of a procfs or sysfs file.
pub(super) fn read_file(path: &CStr) -> Result<Vec<u8>, Errno> {
    let mut file = sys::open(
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut text = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut text).map_err(|error| sys::errno_of(&error))?;
    Ok(text)
}

/// Reads the first line of a file, at most `limit - 1` bytes of it, as the C probe read into a
/// `limit`-byte buffer with room for its NUL.
pub(super) fn read_first_line(path: &CStr, limit: usize) -> Result<Vec<u8>, Errno> {
    let text = read_file(path)?;
    let line = text.split(|byte| *byte == b'\n').next().unwrap_or_default();
    Ok(line[..line.len().min(limit.saturating_sub(1))].to_vec())
}

/// Reads the whole of a procfs or sysfs file, or nothing when it cannot be read.
pub(super) fn read_or_empty(path: &CStr) -> Vec<u8> {
    read_file(path).unwrap_or_default()
}

/// Parses a decimal number in the form in which the kernel prints rates and times, such as
/// `2194.843` or a syslog timestamp's `    3.000000`: digits with an optional fraction, after
/// optional spaces and tabs.
fn parse_kernel_decimal(text: &[u8]) -> Option<f64> {
    let start = text
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(text.len());
    let number = &text[start..];
    let (whole, fraction) = match number.iter().position(|byte| *byte == b'.') {
        Some(dot) => (&number[..dot], &number[dot + 1..]),
        None => (number, &[][..]),
    };
    if whole.is_empty() || !whole.iter().chain(fraction).all(u8::is_ascii_digit) {
        return None;
    }
    parse_f64(number)
}

/// Iterates structured nonblocking `/dev/kmsg` records.
fn kmsg_from_dev(mut consume: impl FnMut(u64, &[u8])) -> Result<(), Errno> {
    let log = sys::open(
        kernel::KERNEL_LOG_DEVICE,
        OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut count = 0;
    let mut record = [0_u8; KMSG_RECORD_MAX];
    let error = loop {
        let read = match log.read(&mut record[..KMSG_RECORD_MAX - 1]) {
            Ok(0) => break Errno::ENODATA,
            Ok(read) => read,
            // A record that the kernel overwrote is skipped, as the C probe skipped it.
            Err(Errno::EINTR | Errno::EPIPE) => continue,
            Err(error) => break error,
        };
        let record = &record[..read];
        let Some(semicolon) = record.iter().position(|byte| *byte == b';') else {
            continue;
        };
        // A record's fields are its priority, sequence number, and microseconds since boot.
        let timestamp = record[..semicolon]
            .split(|byte| *byte == b',')
            .nth(2)
            .map_or(0, parse_decimal_prefix);
        let message = &record[semicolon + 1..];
        let end = message
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(message.len());
        consume(timestamp, &message[..end]);
        count += 1;
    };
    if count > 0 { Ok(()) } else { Err(error) }
}

/// Parses the leading decimal portion of a kernel-log metadata field.
fn parse_decimal_prefix(text: &[u8]) -> u64 {
    let digits = text
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    text[..digits].iter().fold(0_u64, |value, &byte| {
        value
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'))
    })
}

/// Iterates the complete syslog buffer when `/dev/kmsg` yields no records.
fn kmsg_from_syslog(mut consume: impl FnMut(u64, &[u8])) -> Result<(), Errno> {
    let size = sys::kernel::kernel_log_size()
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(FALLBACK_LOG_SIZE);
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
    buffer.resize(size, 0);
    let count = sys::kernel::read_kernel_log(&mut buffer)?;
    for line in buffer[..count].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut message = line;
        if message.first() == Some(&b'<')
            && let Some(close) = message.iter().position(|byte| *byte == b'>')
        {
            message = &message[close + 1..];
        }
        let mut usec = 0;
        if message.first() == Some(&b'[')
            && let Some(close) = message.iter().position(|byte| *byte == b']')
        {
            if let Some(seconds) = parse_kernel_decimal(&message[1..close]) {
                usec = (seconds * MICROSECONDS_PER_SECOND + 0.5) as u64;
            }
            message = &message[close + 1..];
            if message.first() == Some(&b' ') {
                message = &message[1..];
            }
        }
        consume(usec, message);
    }
    Ok(())
}

/// Iterates kernel messages, preferring structured `/dev/kmsg`.
pub(super) fn kmsg_foreach(mut consume: impl FnMut(u64, &[u8])) -> Result<(), Errno> {
    if kmsg_from_dev(&mut consume).is_ok() {
        Ok(())
    } else {
        kmsg_from_syslog(consume)
    }
}

/// Reads `/proc/cpuinfo` once, as raw bytes, which are empty when it cannot be read.
fn cpuinfo() -> &'static [u8] {
    static TEXT: OnceLock<Vec<u8>> = OnceLock::new();
    TEXT.get_or_init(|| read_or_empty(kernel::CPU_INFORMATION))
}

/// Returns the value of the first `/proc/cpuinfo` line whose key starts with `key`.
fn cpuinfo_value(key: &[u8]) -> Option<&'static [u8]> {
    cpuinfo()
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(key))
        .find_map(|line| {
            let colon = line.iter().position(|byte| *byte == b':')?;
            Some(&line[colon + 1..])
        })
}

/// Lazily extracts CPU-zero feature-flag words.
fn cpu_flags() -> &'static [Vec<u8>] {
    static FLAGS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FLAGS.get_or_init(|| {
        cpuinfo_value(b"flags").map_or_else(Vec::new, |words| {
            words
                .split(u8::is_ascii_whitespace)
                .filter(|word| !word.is_empty())
                .map(<[u8]>::to_vec)
                .collect()
        })
    })
}

/// Returns whether cpuinfo contains one exact feature flag.
pub(super) fn cpuinfo_has_flag(flag: &[u8]) -> bool {
    cpu_flags().iter().any(|word| word == flag)
}

/// Returns the first cpuinfo MHz value, or zero.
fn cpuinfo_mhz() -> f64 {
    cpuinfo_value(b"cpu MHz")
        .and_then(parse_kernel_decimal)
        .unwrap_or(0.0)
}

/// Parses an MHz value between a kernel-message prefix and suffix, or returns zero.
fn mhz_after(text: &[u8], prefix: &[u8], suffix: &[u8]) -> f64 {
    let Some(start) = find(text, prefix).map(|start| start + prefix.len()) else {
        return 0.0;
    };
    let value = &text[start..];
    find(value, suffix)
        .and_then(|end| parse_kernel_decimal(&value[..end]))
        .unwrap_or(0.0)
}

/// Finds the first byte-exact occurrence of `needle`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Converts MHz to rounded kHz and then to hertz like the C probe.
fn mhz_to_hz(mhz: f64) -> f64 {
    (mhz * THOUSAND + 0.5) as u64 as f64 * THOUSAND
}

/// Measures the TSC rate against the raw monotonic clock over `interval`.
pub(super) fn measure_tsc_hz(interval: Duration) -> f64 {
    let first = sys::clock_time(sys::Clock::MonotonicRaw).unwrap_or_default();
    let t0 = sys::x86::rdtsc_fenced();
    sys::sleep_once(interval);
    let second = sys::clock_time(sys::Clock::MonotonicRaw).unwrap_or_default();
    let t1 = sys::x86::rdtsc_fenced();
    let ns = (second.seconds - first.seconds) as f64 * NANOSECONDS_PER_SECOND
        + (second.nanoseconds - first.nanoseconds) as f64;
    if ns > 0.0 {
        t1.wrapping_sub(t0) as f64 * NANOSECONDS_PER_SECOND / ns
    } else {
        0.0
    }
}

/// Detects TSC rate in option, sysfs, kernel-log, cpuinfo, then measured order.
pub(super) fn detect_tsc_rate(override_hz: f64) -> TscRate {
    if override_hz > 0.0 {
        return TscRate {
            hz: override_hz,
            source: "option",
        };
    }
    if let Ok(line) = read_first_line(kernel::TSC_FREQUENCY_KHZ, VALUE_LIMIT)
        && let Some(khz) = parse_decimal(&line).filter(|value| *value > 0)
    {
        return TscRate {
            hz: khz as f64 * THOUSAND,
            source: "sysfs-tsc_freq_khz",
        };
    }
    let mut refined = 0.0;
    let mut detected_tsc = 0.0;
    let mut detected_cpu = 0.0;
    let _ = kmsg_foreach(|_, text| {
        let value = mhz_after(text, b"Refined TSC clocksource calibration: ", b" MHz");
        if value > 0.0 {
            refined = value;
            return;
        }
        let value = mhz_after(text, b"tsc: Detected ", b" MHz TSC");
        if value > 0.0 {
            detected_tsc = value;
            return;
        }
        let value = mhz_after(text, b"tsc: Detected ", b" MHz processor");
        if value > 0.0 {
            detected_cpu = value;
        }
    });
    for (mhz, source) in [
        (refined, "kmsg-refined"),
        (detected_tsc, "kmsg-detected-tsc"),
        (detected_cpu, "kmsg-detected-processor"),
    ] {
        if mhz > 0.0 {
            return TscRate {
                hz: mhz_to_hz(mhz),
                source,
            };
        }
    }
    let mhz = cpuinfo_mhz();
    if !cpuinfo_has_flag(b"aperfmperf") && mhz > 0.0 {
        return TscRate {
            hz: mhz_to_hz(mhz),
            source: "cpuinfo-cpu-mhz",
        };
    }
    TscRate {
        hz: measure_tsc_hz(MEASURE_INTERVAL),
        source: "measured-monotonic-raw",
    }
}

/// Reads the active clocksource token.
pub(super) fn current_clocksource() -> Vec<u8> {
    read_first_line(kernel::CURRENT_CLOCKSOURCE, VALUE_LIMIT)
        .unwrap_or_else(|_| b"unknown".to_vec())
}

/// Converts TSC cycles to nanoseconds using `rate`.
pub(super) fn cycles_to_ns(cycles: f64, rate: TscRate) -> f64 {
    if rate.hz > 0.0 {
        cycles * NANOSECONDS_PER_SECOND / rate.hz
    } else {
        0.0
    }
}

/// Rounds a floating-point value with C `llround` half-away-from-zero behavior.
pub(super) fn round_ll(value: f64) -> i64 {
    value.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_reject_duplicates_and_format_ranges() {
        assert_eq!(parse_cpu_list(b"0-3,6"), Some(vec![0, 1, 2, 3, 6]));
        assert_eq!(parse_cpu_list(b""), Some(Vec::new()));
        assert_eq!(format_cpu_list(&[0, 1, 2, 3, 6]), "0-3,6");
        for list in [
            &b"0,0"[..],
            b"3-1",
            b"1024",
            b"0,",
            b",0",
            b"-1",
            b"+1",
            b" 1",
            b"1-",
            b"0x1",
        ] {
            assert!(parse_cpu_list(list).is_none(), "{}", list.escape_ascii());
        }
    }

    #[test]
    fn numeric_options_are_strict() {
        assert_eq!(parse_u64(b"0x10"), Some(16));
        assert_eq!(parse_u64(b"10"), Some(10));
        assert!(parse_u64(b"010").is_none());
        assert!(parse_u64(b"1x").is_none());
        assert_eq!(parse_rate(b"2.5"), Some(2.5));
        assert_eq!(parse_rate(b"1e200"), Some(1e200));
        assert!(parse_rate(b"0").is_none());
        assert!(parse_rate(b"2.5x").is_none());
    }

    #[test]
    fn kernel_numbers_are_read_in_the_kernels_form() {
        assert_eq!(parse_kernel_decimal(b" 2194.843"), Some(2194.843));
        assert_eq!(parse_kernel_decimal(b"    3.000000"), Some(3.0));
        assert_eq!(parse_kernel_decimal(b"\t2194"), Some(2194.0));
        assert_eq!(parse_kernel_decimal(b"-1.5"), None);
        assert_eq!(parse_kernel_decimal(b"1e3"), None);
        assert_eq!(parse_kernel_decimal(b"2194.843 "), None);
        assert_eq!(
            mhz_after(
                b"tsc: Detected 2194.843 MHz processor",
                b"tsc: Detected ",
                b" MHz processor"
            ),
            2194.843
        );
        assert_eq!(
            mhz_after(
                b"tsc: Detected 2194.843 MHz TSC",
                b"tsc: Detected ",
                b" MHz processor"
            ),
            0.0
        );
        assert_eq!(mhz_to_hz(2194.843), 2_194_843_000.0);
        assert_eq!(parse_decimal_prefix(b"1234,-"), 1234);
    }

    #[test]
    fn nearest_integer_rounding_matches_the_probe() {
        assert_eq!(round_ll(1.49), 1);
        assert_eq!(round_ll(1.5), 2);
        assert_eq!(round_ll(-1.5), -2);
        assert_eq!(round_ll(-0.4), 0);
    }
}
