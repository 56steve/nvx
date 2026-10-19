//! The published `/run/nvx/time/state` representation.

use std::fmt::{self, Write as _};
use std::str::FromStr;

use super::Phase;

/// Defines an enumeration of the tokens of one state-file field, with each token's spelling.
macro_rules! tokens {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $token:literal,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            /// Returns the token that `text` spells.
            fn parse(text: &str) -> Option<Self> {
                match text {
                    $($token => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            /// Writes the token's spelling.
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(match self {
                    $(Self::$variant => $token,)+
                })
            }
        }
    };
}

tokens! {
    /// The outcome of one phase's conformance checks.
    CheckStatus {
        /// The checks have not finished.
        Pending => "pending",
        /// The checks passed.
        Ok => "ok",
        /// A check failed in report-only mode, which records failures.
        Fail => "fail",
    }
}

tokens! {
    /// The latest discontinuity of the wall clock.
    #[derive(Default)]
    Discontinuity {
        /// None since boot.
        #[default]
        None => "none",
        /// The discipline stepped the clock.
        Step => "step",
        /// A restore's repair stepped the clock.
        Restore => "restore",
    }
}

tokens! {
    /// The host clock that measured the latest downtime.
    #[derive(Default)]
    DowntimeSource {
        /// No downtime since boot.
        #[default]
        None => "none",
        /// The host's UTC clock.
        Utc => "utc",
        /// The host's monotonic clock.
        Monotonic => "monotonic",
    }
}

tokens! {
    /// Why the discipline rejected its latest time sample.
    #[derive(Default)]
    SampleRejection {
        /// The discipline accepted the latest sample.
        #[default]
        None => "none",
        /// No sample was narrow enough.
        Uncertain => "G_SAMPLE_UNCERTAIN",
    }
}

/// Persisted outcome and accounting fields for one check phase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CheckState {
    /// The outcome, or `None` until the phase's checks first run.
    pub(crate) status: Option<CheckStatus>,
    /// Number of CPUs covered by the phase.
    pub(crate) cpus: u64,
    /// Monotonic elapsed time in microseconds.
    pub(crate) elapsed_us: i64,
    /// Process CPU time in microseconds.
    pub(crate) cpu_us: i64,
    /// VM generation to which this phase applies.
    pub(crate) generation: u64,
    /// Number of conformance failures, which only report-only mode records.
    pub(crate) failures: Option<u64>,
}

/// Complete version-one state published under `/run/nvx/time/state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimeState {
    /// Current VM generation.
    pub(crate) generation: u64,
    /// Boot, capture, and restore records, in the order of `Phase::CHECKS`.
    checks: [CheckState; 3],
    /// Validated TSC frequency in hertz.
    pub(crate) tsc_hz: u64,
    /// Validated local-APIC timer frequency in hertz.
    pub(crate) lapic_hz: u64,
    /// Number of observed clock discontinuities.
    pub(crate) discontinuities: u64,
    /// The latest discontinuity.
    pub(crate) last_discontinuity: Discontinuity,
    /// Signed nanoseconds applied by the latest step.
    pub(crate) last_step_ns: i64,
    /// Realtime clock immediately after the latest step.
    pub(crate) last_step_realtime_ns: i64,
    /// Most recently reported VM downtime in nanoseconds.
    pub(crate) last_downtime_ns: u64,
    /// The host clock that measured the latest downtime.
    pub(crate) last_downtime_source: DowntimeSource,
    /// Whether recent accepted samples keep the clock synchronized.
    pub(crate) synchronized: bool,
    /// Latest accepted guest-to-host clock offset in nanoseconds.
    pub(crate) offset_ns: i64,
    /// Latest accepted sample uncertainty in nanoseconds.
    pub(crate) uncertainty_ns: i64,
    /// Current frequency correction in parts per billion.
    pub(crate) frequency_ppb: i64,
    /// Number of accepted time samples.
    pub(crate) samples: u64,
    /// Number of rejected time samples.
    pub(crate) rejected_samples: u64,
    /// Why the latest time sample was rejected.
    pub(crate) last_sample_error: SampleRejection,
    /// Number of emitted ABI violations.
    pub(crate) violations: u64,
}

impl Default for TimeState {
    /// Returns a fresh state, whose only record is the pending boot check.
    fn default() -> Self {
        let boot = CheckState {
            status: Some(CheckStatus::Pending),
            ..CheckState::default()
        };
        Self {
            generation: 0,
            checks: [boot, CheckState::default(), CheckState::default()],
            tsc_hz: 0,
            lapic_hz: 0,
            discontinuities: 0,
            last_discontinuity: Discontinuity::None,
            last_step_ns: 0,
            last_step_realtime_ns: 0,
            last_downtime_ns: 0,
            last_downtime_source: DowntimeSource::None,
            synchronized: false,
            offset_ns: 0,
            uncertainty_ns: 0,
            frequency_ppb: 0,
            samples: 0,
            rejected_samples: 0,
            last_sample_error: SampleRejection::None,
            violations: 0,
        }
    }
}

impl TimeState {
    /// Parses the state file's `key=value` lines, ignoring unknown keys, or returns `None` for a
    /// known key whose value is malformed.
    pub(crate) fn parse(text: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(text).ok()?;
        let mut state = Self::default();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            state.set(key, value)?;
        }
        Some(state)
    }

    /// Sets the field that `key` names to `value`, or returns `None` for a malformed value. An
    /// unknown key changes nothing.
    fn set(&mut self, key: &str, value: &str) -> Option<()> {
        /// Parses a decimal number.
        fn number<T: FromStr>(value: &str) -> Option<T> {
            value.parse().ok()
        }
        let check = Phase::CHECKS
            .into_iter()
            .zip(&mut self.checks)
            .find_map(|(phase, check)| {
                let field = key.strip_prefix(phase.name())?.strip_prefix('_')?;
                Some((phase, check, field))
            });
        if let Some((phase, check, field)) = check {
            match field {
                "status" => check.status = Some(CheckStatus::parse(value)?),
                "cpus" => check.cpus = number(value)?,
                "elapsed_us" => check.elapsed_us = number(value)?,
                "cpu_us" => check.cpu_us = number(value)?,
                // The boot check always applies to the boot generation, which the file omits.
                "generation" if phase != Phase::Boot => check.generation = number(value)?,
                "failures" => check.failures = Some(number(value)?),
                _ => {}
            }
            return Some(());
        }
        match key {
            "generation" => self.generation = number(value)?,
            "tsc_hz" => self.tsc_hz = number(value)?,
            "lapic_hz" => self.lapic_hz = number(value)?,
            "discontinuities" => self.discontinuities = number(value)?,
            "last_discontinuity" => self.last_discontinuity = Discontinuity::parse(value)?,
            "last_step_ns" => self.last_step_ns = number(value)?,
            "last_step_realtime_ns" => self.last_step_realtime_ns = number(value)?,
            "last_downtime_ns" => self.last_downtime_ns = number(value)?,
            "last_downtime_source" => self.last_downtime_source = DowntimeSource::parse(value)?,
            "synchronized" => self.synchronized = number::<u64>(value)? != 0,
            "offset_ns" => self.offset_ns = number(value)?,
            "uncertainty_ns" => self.uncertainty_ns = number(value)?,
            "frequency_ppb" => self.frequency_ppb = number(value)?,
            "samples" => self.samples = number(value)?,
            "rejected_samples" => self.rejected_samples = number(value)?,
            "last_sample_error" => self.last_sample_error = SampleRejection::parse(value)?,
            "violations" => self.violations = number(value)?,
            _ => {}
        }
        Some(())
    }

    /// Returns the record of `phase`, unless it is the runtime phase, which has none.
    pub(crate) fn check(&self, phase: Phase) -> Option<&CheckState> {
        self.checks.get(phase.check_index()?)
    }

    /// Returns the record of `phase`, unless it is the runtime phase, which has none.
    pub(crate) fn check_mut(&mut self, phase: Phase) -> Option<&mut CheckState> {
        self.checks.get_mut(phase.check_index()?)
    }

    /// Returns the phases whose checks have run, with their records and outcomes, in state-file
    /// order.
    fn recorded(&self) -> impl Iterator<Item = (Phase, &CheckState, CheckStatus)> {
        Phase::CHECKS
            .into_iter()
            .zip(&self.checks)
            .filter_map(|(phase, check)| Some((phase, check, check.status?)))
    }

    /// Returns whether the boot check, or unless `boot_only` any recorded check, is pending.
    pub(crate) fn pending(&self, boot_only: bool) -> bool {
        self.recorded()
            .filter(|&(phase, ..)| !boot_only || phase == Phase::Boot)
            .any(|(.., status)| status == CheckStatus::Pending)
    }

    /// Returns whether every recorded check passed.
    pub(crate) fn checks_ok(&self) -> bool {
        self.recorded()
            .all(|(.., status)| status == CheckStatus::Ok)
    }

    /// Formats the status markers: one per recorded check, then the discipline's.
    pub(crate) fn status_markers(&self, report_only: bool) -> String {
        let prefix = if report_only {
            "NVX-TIME-REPORT"
        } else {
            "NVX-TIME-ABI"
        };
        let mut text = String::new();
        for (phase, check, status) in self.recorded() {
            let _ = write!(
                text,
                "{prefix}: v=1 phase={} status={status} cpus={} tsc_hz={} lapic_hz={} \
                 generation={}",
                phase.name(),
                check.cpus,
                self.tsc_hz,
                self.lapic_hz,
                check.generation
            );
            if status != CheckStatus::Pending {
                let _ = write!(
                    text,
                    " elapsed_us={} cpu_us={}",
                    check.elapsed_us, check.cpu_us
                );
            }
            if report_only {
                let _ = write!(text, " failures={}", check.failures.unwrap_or(0));
            }
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "{prefix}: v=1 phase=runtime status={} generation={} discontinuities={} \
             offset_ns={} uncertainty_ns={} rejected_samples={} last_sample_error={}",
            if self.synchronized {
                "synchronized"
            } else {
                "unsynchronized"
            },
            self.generation,
            self.discontinuities,
            self.offset_ns,
            self.uncertainty_ns,
            self.rejected_samples,
            self.last_sample_error
        );
        text
    }
}

impl fmt::Display for TimeState {
    /// Writes the state file, with its keys in the order that the ABI fixes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "version=1")?;
        writeln!(formatter, "generation={}", self.generation)?;
        for (phase, _, status) in self.recorded() {
            writeln!(formatter, "{}_status={status}", phase.name())?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(formatter, "{}_cpus={}", phase.name(), check.cpus)?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(
                formatter,
                "{}_elapsed_us={}",
                phase.name(),
                check.elapsed_us
            )?;
        }
        for (phase, check, _) in self.recorded() {
            writeln!(formatter, "{}_cpu_us={}", phase.name(), check.cpu_us)?;
        }
        for (phase, check, _) in self.recorded().filter(|&(phase, ..)| phase != Phase::Boot) {
            writeln!(
                formatter,
                "{}_generation={}",
                phase.name(),
                check.generation
            )?;
        }
        for (phase, check, _) in self.recorded() {
            if let Some(failures) = check.failures {
                writeln!(formatter, "{}_failures={failures}", phase.name())?;
            }
        }
        writeln!(formatter, "tsc_hz={}", self.tsc_hz)?;
        writeln!(formatter, "lapic_hz={}", self.lapic_hz)?;
        writeln!(formatter, "discontinuities={}", self.discontinuities)?;
        writeln!(formatter, "last_discontinuity={}", self.last_discontinuity)?;
        writeln!(formatter, "last_step_ns={}", self.last_step_ns)?;
        writeln!(
            formatter,
            "last_step_realtime_ns={}",
            self.last_step_realtime_ns
        )?;
        writeln!(formatter, "last_downtime_ns={}", self.last_downtime_ns)?;
        writeln!(
            formatter,
            "last_downtime_source={}",
            self.last_downtime_source
        )?;
        writeln!(formatter, "synchronized={}", u8::from(self.synchronized))?;
        writeln!(formatter, "offset_ns={}", self.offset_ns)?;
        writeln!(formatter, "uncertainty_ns={}", self.uncertainty_ns)?;
        writeln!(formatter, "frequency_ppb={}", self.frequency_ppb)?;
        writeln!(formatter, "samples={}", self.samples)?;
        writeln!(formatter, "rejected_samples={}", self.rejected_samples)?;
        writeln!(formatter, "last_sample_error={}", self.last_sample_error)?;
        writeln!(formatter, "violations={}", self.violations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUND_TRIP_STATE: &str = concat!(
        "version=1\n",
        "generation=2\n",
        "boot_status=ok\n",
        "capture_status=ok\n",
        "restore_status=pending\n",
        "boot_cpus=8\n",
        "capture_cpus=8\n",
        "restore_cpus=0\n",
        "boot_elapsed_us=2400\n",
        "capture_elapsed_us=310\n",
        "restore_elapsed_us=1830\n",
        "boot_cpu_us=1900\n",
        "capture_cpu_us=290\n",
        "restore_cpu_us=640\n",
        "capture_generation=1\n",
        "restore_generation=2\n",
        "tsc_hz=2194843000\n",
        "lapic_hz=200000000\n",
        "discontinuities=3\n",
        "last_discontinuity=step\n",
        "last_step_ns=-200000000\n",
        "last_step_realtime_ns=1790841600000000000\n",
        "last_downtime_ns=31000000000\n",
        "last_downtime_source=utc\n",
        "synchronized=1\n",
        "offset_ns=-42\n",
        "uncertainty_ns=1800\n",
        "frequency_ppb=-1200\n",
        "samples=17\n",
        "rejected_samples=1\n",
        "last_sample_error=none\n",
        "violations=0\n"
    );

    const STATUS_STATE: &str = concat!(
        "version=1\n",
        "generation=2\n",
        "boot_status=ok\n",
        "capture_status=ok\n",
        "restore_status=ok\n",
        "boot_cpus=8\n",
        "capture_cpus=8\n",
        "restore_cpus=4\n",
        "boot_elapsed_us=2400\n",
        "capture_elapsed_us=310\n",
        "restore_elapsed_us=1830\n",
        "boot_cpu_us=1900\n",
        "capture_cpu_us=290\n",
        "restore_cpu_us=640\n",
        "capture_generation=1\n",
        "restore_generation=2\n",
        "tsc_hz=2194843000\n",
        "lapic_hz=200000000\n",
        "discontinuities=2\n",
        "synchronized=1\n",
        "offset_ns=-42\n",
        "uncertainty_ns=1800\n",
        "rejected_samples=2\n",
        "last_sample_error=G_SAMPLE_UNCERTAIN\n"
    );

    fn status(text: &str, report_only: bool) -> String {
        let state = TimeState::parse(text.as_bytes()).unwrap();
        let mut output = state.status_markers(report_only);
        output.push_str(&format!("ok={}\n", u8::from(state.checks_ok())));
        output
    }

    #[test]
    fn fresh_state_has_only_a_pending_boot_check() {
        let state = TimeState::parse(b"version=1\n").unwrap();
        assert!(
            state
                .to_string()
                .starts_with("version=1\ngeneration=0\nboot_status=pending\nboot_cpus=0\n")
        );
        assert!(state.to_string().starts_with(
            "version=1\ngeneration=0\nboot_status=pending\nboot_cpus=0\n\
                 boot_elapsed_us=0\nboot_cpu_us=0\ntsc_hz=0\nlapic_hz=0\n"
        ));
        assert_eq!(state.checks[1].status, None);
        assert!(state.pending(true) && state.pending(false) && !state.checks_ok());
    }

    #[test]
    fn malformed_known_values_fail_but_unknown_keys_do_not() {
        for text in [
            &b"unknown=x\nsamples=bad\n"[..],
            b"boot_status=done\n",
            b"last_discontinuity=jump\n",
            b"generation=-1\n",
            b"boot_failures=-1\n",
            b"samples=\xff\n",
        ] {
            assert_eq!(TimeState::parse(text), None, "{}", text.escape_ascii());
        }
        assert!(TimeState::parse(b"unknown=bad\n").is_some());
    }

    #[test]
    fn records_round_trip_with_their_tokens() {
        let text = "version=1\ngeneration=2\nboot_status=ok\nrestore_status=fail\nboot_cpus=8\n\
                    restore_cpus=4\nboot_elapsed_us=1\nrestore_elapsed_us=2\nboot_cpu_us=3\n\
                    restore_cpu_us=4\nrestore_generation=2\nrestore_failures=3\ntsc_hz=0\n\
                    lapic_hz=0\ndiscontinuities=1\nlast_discontinuity=restore\nlast_step_ns=-5\n\
                    last_step_realtime_ns=6\nlast_downtime_ns=7\nlast_downtime_source=monotonic\n\
                    synchronized=1\noffset_ns=-8\nuncertainty_ns=9\nfrequency_ppb=-10\n\
                    samples=11\nrejected_samples=12\nlast_sample_error=G_SAMPLE_UNCERTAIN\n\
                    violations=13\n";
        let state = TimeState::parse(text.as_bytes()).unwrap();
        assert_eq!(state.to_string(), text);
        assert!(!state.pending(false) && !state.checks_ok());
        assert_eq!(
            state.status_markers(true).lines().next(),
            Some(
                "NVX-TIME-REPORT: v=1 phase=boot status=ok cpus=8 tsc_hz=0 lapic_hz=0 \
                 generation=0 elapsed_us=1 cpu_us=3 failures=0"
            )
        );
    }

    #[test]
    fn enforcing_and_report_only_states_round_trip_exactly() {
        let state = TimeState::parse(ROUND_TRIP_STATE.as_bytes()).unwrap();
        assert_eq!(state.to_string(), ROUND_TRIP_STATE);
        assert!(state.pending(false) && !state.checks_ok());

        let report = ROUND_TRIP_STATE
            .replace("restore_status=pending", "restore_status=fail")
            .replace(
                "restore_generation=2\n",
                "restore_generation=2\nboot_failures=0\ncapture_failures=0\n\
                 restore_failures=3\n",
            );
        assert_eq!(
            TimeState::parse(report.as_bytes()).unwrap().to_string(),
            report
        );
    }

    #[test]
    fn status_prints_every_recorded_check_and_the_runtime_line() {
        const RATES: &str = "tsc_hz=2194843000 lapic_hz=200000000";
        assert_eq!(
            status(STATUS_STATE, false),
            format!(
                "NVX-TIME-ABI: v=1 phase=boot status=ok cpus=8 {RATES} \
                 generation=0 elapsed_us=2400 cpu_us=1900\n\
                 NVX-TIME-ABI: v=1 phase=capture status=ok cpus=8 {RATES} \
                 generation=1 elapsed_us=310 cpu_us=290\n\
                 NVX-TIME-ABI: v=1 phase=restore status=ok cpus=4 {RATES} \
                 generation=2 elapsed_us=1830 cpu_us=640\n\
                 NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=2 \
                 discontinuities=2 offset_ns=-42 uncertainty_ns=1800 rejected_samples=2 \
                 last_sample_error=G_SAMPLE_UNCERTAIN\n\
                 ok=1\n"
            )
        );

        let pending = status(
            &STATUS_STATE.replace("restore_status=ok", "restore_status=pending"),
            false,
        );
        assert!(pending.contains(&format!(
            "NVX-TIME-ABI: v=1 phase=restore status=pending cpus=4 {RATES} generation=2\n"
        )));
        assert!(pending.ends_with("ok=0\n"));

        let fresh = concat!(
            "NVX-TIME-ABI: v=1 phase=boot status=pending cpus=0 tsc_hz=0 ",
            "lapic_hz=0 generation=0\n",
            "NVX-TIME-ABI: v=1 phase=runtime status=unsynchronized generation=0 ",
            "discontinuities=0 offset_ns=0 uncertainty_ns=0 rejected_samples=0 ",
            "last_sample_error=none\n",
            "ok=0\n"
        );
        for text in [
            "version=1\ngeneration=0\nboot_status=pending\n",
            "version=1\n",
        ] {
            assert_eq!(status(text, false), fresh, "{text:?}");
        }

        let report_state = STATUS_STATE
            .replace("restore_status=ok", "restore_status=fail")
            .replace(
                "restore_generation=2\n",
                "restore_generation=2\nboot_failures=0\nrestore_failures=3\n",
            );
        assert_eq!(
            status(&report_state, true),
            format!(
                "NVX-TIME-REPORT: v=1 phase=boot status=ok cpus=8 {RATES} \
                 generation=0 elapsed_us=2400 cpu_us=1900 failures=0\n\
                 NVX-TIME-REPORT: v=1 phase=capture status=ok cpus=8 {RATES} \
                 generation=1 elapsed_us=310 cpu_us=290 failures=0\n\
                 NVX-TIME-REPORT: v=1 phase=restore status=fail cpus=4 {RATES} \
                 generation=2 elapsed_us=1830 cpu_us=640 failures=3\n\
                 NVX-TIME-REPORT: v=1 phase=runtime status=synchronized generation=2 \
                 discontinuities=2 offset_ns=-42 uncertainty_ns=1800 rejected_samples=2 \
                 last_sample_error=G_SAMPLE_UNCERTAIN\n\
                 ok=0\n"
            )
        );
    }
}
