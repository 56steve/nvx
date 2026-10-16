//! The x86-64 instructions that the time tools execute, through the `core::arch` intrinsics.
//!
//! The `CPUID` intrinsic compiles to the single instruction, which is safe to execute in user
//! mode on every x86-64 processor. It became a safe function after the crate's minimum Rust
//! version, so its call allows an unused `unsafe` block.

use core::arch::x86_64::{__cpuid_count, CpuidResult};

/// CPUID leaves of the architecture and of Hyper-V, and the bits of their registers, that the
/// time tools read.
pub(crate) mod cpuid {
    /// Basic leaf whose EAX is the highest basic leaf.
    pub(crate) const LEAF_MAXIMUM: u32 = 0;
    /// Basic leaf of the processor's signature and feature flags.
    pub(crate) const LEAF_FEATURES: u32 = 1;
    /// Basic leaf of thermal and power management, whose EAX bit 2 is ARAT.
    pub(crate) const LEAF_POWER_MANAGEMENT: u32 = 6;
    /// Basic leaf of the structured extended features, read with subleaf 0.
    pub(crate) const LEAF_STRUCTURED_FEATURES: u32 = 7;
    /// Basic leaf of architectural performance monitoring.
    pub(crate) const LEAF_PERFORMANCE_MONITORING: u32 = 0x0a;
    /// Basic leaf of the time-stamp counter's ratio to the core crystal clock.
    pub(crate) const LEAF_TSC_CRYSTAL: u32 = 0x15;
    /// Basic leaf of the processor's base, maximum, and bus frequencies.
    pub(crate) const LEAF_FREQUENCY: u32 = 0x16;
    /// Extended leaf whose EAX is the highest extended leaf.
    pub(crate) const LEAF_EXTENDED_MAXIMUM: u32 = 0x8000_0000;
    /// Extended leaf of the extended feature flags.
    pub(crate) const LEAF_EXTENDED_FEATURES: u32 = 0x8000_0001;
    /// Extended leaf of advanced power management, whose EDX bit 8 is the invariant TSC.
    pub(crate) const LEAF_ADVANCED_POWER: u32 = 0x8000_0007;
    /// First leaf of the hypervisor range, whose EAX is the highest hypervisor leaf and whose
    /// EBX, ECX, and EDX hold the hypervisor's vendor signature.
    pub(crate) const LEAF_HYPERVISOR: u32 = 0x4000_0000;

    /// Leaf 1 ECX bit set when the processor runs under a hypervisor.
    pub(crate) const FEATURES_ECX_HYPERVISOR: u32 = 31;
    /// Leaf 1 ECX bit of the local APIC's TSC-deadline timer mode.
    pub(crate) const FEATURES_ECX_TSC_DEADLINE: u32 = 24;
    /// Leaf 1 ECX bit of the performance and debug capability MSR.
    pub(crate) const FEATURES_ECX_PDCM: u32 = 15;
    /// Leaf 1 EDX bit of the time-stamp counter.
    pub(crate) const FEATURES_EDX_TSC: u32 = 4;
    /// Leaf 6 EAX bit of the always-running APIC timer.
    pub(crate) const POWER_MANAGEMENT_EAX_ARAT: u32 = 2;
    /// Leaf 7 subleaf 0 EBX bit of the `IA32_TSC_ADJUST` MSR.
    pub(crate) const STRUCTURED_FEATURES_EBX_TSC_ADJUST: u32 = 1;
    /// Leaf 0x80000001 EDX bit of the `RDTSCP` instruction.
    pub(crate) const EXTENDED_FEATURES_EDX_RDTSCP: u32 = 27;
    /// Leaf 0x80000007 EDX bit of the invariant time-stamp counter.
    pub(crate) const ADVANCED_POWER_EDX_INVARIANT_TSC: u32 = 8;

    /// Hyper-V features EAX bit of the hypercall MSRs.
    pub(crate) const HYPERV_EAX_HYPERCALL: u32 = 5;
    /// Hyper-V features EAX bit of the virtual-processor index MSR.
    pub(crate) const HYPERV_EAX_VP_INDEX: u32 = 6;
    /// Hyper-V features EAX bit of the TSC and APIC frequency MSRs.
    pub(crate) const HYPERV_EAX_ACCESS_FREQUENCY: u32 = 11;
    /// Hyper-V features EAX bit of the TSC invariant-control MSR.
    pub(crate) const HYPERV_EAX_ACCESS_TSC_INVARIANT: u32 = 15;
    /// Hyper-V features EDX bit that makes the frequency MSRs available.
    pub(crate) const HYPERV_EDX_FREQUENCY_MSRS: u32 = 8;

    /// Returns whether bit `bit` of `register` is set.
    pub(crate) const fn bit(register: u32, bit: u32) -> bool {
        (register >> bit) & 1 != 0
    }
}

/// Model-specific registers of the architecture and of Hyper-V that the time tools read or
/// write.
pub(crate) mod msr {
    /// `IA32_TSC_ADJUST`.
    pub(crate) const IA32_TSC_ADJUST: u32 = 0x3b;
    /// `IA32_TSC_DEADLINE`.
    pub(crate) const IA32_TSC_DEADLINE: u32 = 0x6e0;
    /// Hyper-V's `HV_X64_MSR_GUEST_OS_ID`.
    pub(crate) const HV_GUEST_OS_ID: u32 = 0x4000_0000;
    /// Hyper-V's `HV_X64_MSR_HYPERCALL`.
    pub(crate) const HV_HYPERCALL: u32 = 0x4000_0001;
    /// Hyper-V's `HV_X64_MSR_VP_INDEX`.
    pub(crate) const HV_VP_INDEX: u32 = 0x4000_0002;
    /// Hyper-V's `HV_X64_MSR_TIME_REF_COUNT`.
    pub(crate) const HV_TIME_REF_COUNT: u32 = 0x4000_0020;
    /// Hyper-V's `HV_X64_MSR_TSC_FREQUENCY`.
    pub(crate) const HV_TSC_FREQUENCY: u32 = 0x4000_0022;
    /// Hyper-V's `HV_X64_MSR_APIC_FREQUENCY`.
    pub(crate) const HV_APIC_FREQUENCY: u32 = 0x4000_0023;
    /// Hyper-V's `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
    pub(crate) const HV_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;
}

/// Runs `CPUID` with `leaf` and `subleaf` and returns EAX, EBX, ECX, and EDX.
#[allow(unused_unsafe)]
pub(crate) fn cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    // SAFETY: CPUID executes in user mode on every x86-64 processor and accesses no memory.
    let CpuidResult { eax, ebx, ecx, edx } = unsafe { __cpuid_count(leaf, subleaf) };
    [eax, ebx, ecx, edx]
}

#[cfg(test)]
mod tests {
    //! Checks of the instructions on the build host.

    use super::*;

    #[test]
    fn cpuid_reports_the_time_stamp_counter() {
        let features = cpuid(cpuid::LEAF_FEATURES, 0);
        assert!(cpuid::bit(features[3], cpuid::FEATURES_EDX_TSC));
        assert!(!cpuid::bit(0, 0) && cpuid::bit(1 << 31, 31));
    }
}
