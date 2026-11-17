// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exclusive periodic counter hardware. No kernel IRQ, allocation or task APIs.
#![no_std]
#![cfg(target_arch = "x86_64")]

use core::arch::x86_64::__cpuid;

const ENABLE: u64 = 1 << 22;
const INTEL_REF_BIT: u64 = 1 << 34;

/// Validated counter register family on the probing CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Architectural fixed counter 2: unhalted reference cycles.
    IntelReference,
    /// AMD core-cycle event, legacy or extended six-counter register bank.
    Amd { extended: bool, global: bool },
}

/// Capability snapshot; retain CPU pinning between probing and all operations.
#[derive(Clone, Copy, Debug)]
pub struct Counter {
    kind: Kind,
    width: u8,
    counters: u8,
}

impl Counter {
    /// Probes documented vendor/CPUID capabilities without touching PMU MSRs.
    pub fn probe() -> Option<Self> {
        // SAFETY: CPUID is supported on x86_64 and has no memory side effects.
        let vendor = __cpuid(0);
        // SAFETY: the feature leaf exists on every supported x86_64 CPU.
        let feature = __cpuid(1);
        if feature.edx & (1 << 9) == 0 {
            return None;
        }
        if (vendor.ebx, vendor.edx, vendor.ecx) == (0x756e6547, 0x49656e69, 0x6c65746e)
            && vendor.eax >= 0xa
        {
            // SAFETY: the maximum basic leaf validated architectural PMU CPUID.
            let pmu = __cpuid(0xa);
            return Self::intel(pmu.eax, pmu.edx);
        }
        if (vendor.ebx, vendor.edx, vendor.ecx) != (0x68747541, 0x69746e65, 0x444d4163) {
            return None;
        }
        let family = ((feature.eax >> 8) & 0xf) + ((feature.eax >> 20) & 0xff);
        // Bound support to documented core PMUs; older families and future
        // families must not be guessed to implement the same MSR bank.
        if !(0x10..=0x1a).contains(&family) {
            return None;
        }
        // SAFETY: extended maximum CPUID is available on x86_64.
        let maximum = __cpuid(0x8000_0000).eax;
        if maximum < 0x8000_0001 {
            return None;
        }
        // SAFETY: maximum extended leaf includes the requested feature leaf.
        let ext = __cpuid(0x8000_0001);
        let extended = ext.ecx & (1 << 23) != 0;
        let mut counters = if extended { 6 } else { 4 };
        let global = if maximum >= 0x8000_0022 {
            // SAFETY: maximum extended leaf validated PerfMonV2 capability.
            let perf = __cpuid(0x8000_0022);
            if perf.eax & 1 != 0 {
                counters = (perf.ebx & 0xf) as u8;
                if !extended || !(1..=6).contains(&counters) {
                    return None;
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        Some(Self {
            kind: Kind::Amd { extended, global },
            width: 48,
            counters,
        })
    }

    fn intel(eax: u32, edx: u32) -> Option<Self> {
        let width = ((edx >> 5) & 0xff) as u8;
        if eax & 0xff < 2 || edx & 0x1f < 3 || !(32..=64).contains(&width) || (eax >> 8) & 0xff > 32
        {
            return None;
        }
        Some(Self {
            kind: Kind::IntelReference,
            width,
            counters: (edx & 0x1f) as u8,
        })
    }

    /// Counter family selected by capability probing.
    pub const fn kind(self) -> Kind {
        self.kind
    }

    /// Converts a requested period to a nonzero half-width-safe preload.
    /// Saturation requests more frequent samples rather than silently wrapping.
    pub fn preload(self, period_ns: u64, frequency_hz: u64) -> Option<u64> {
        if period_ns == 0 || frequency_hz == 0 {
            return None;
        }
        let maximum = (1u128 << (self.width - 1)) - 1;
        let cycles = (u128::from(period_ns) * u128::from(frequency_hz) / 1_000_000_000)
            .clamp(1, maximum) as u64;
        Some(cycles.wrapping_neg() & self.mask())
    }

    fn mask(self) -> u64 {
        u64::MAX >> (64 - self.width)
    }

    fn amd_control(self) -> u32 {
        if matches!(self.kind, Kind::Amd { extended: true, .. }) {
            0xc001_0200
        } else {
            0xc001_0000
        }
    }

    fn amd_counter(self) -> u32 {
        if matches!(self.kind, Kind::Amd { extended: true, .. }) {
            0xc001_0201
        } else {
            0xc001_0004
        }
    }

    /// Checks exclusive ownership without changing any PMU state.
    ///
    /// # Safety
    /// Run pinned on the probed CPU at CPL0. CPUID must truthfully expose its
    /// PMU MSRs. No other PMU consumer may change ownership during this check.
    pub unsafe fn is_vacant(self) -> bool {
        match self.kind {
            Kind::IntelReference => {
                self.read(0x38f) == 0
                    && self.read(0x38d) == 0
                    && (0..self.counters).all(|index| self.read(0x186 + u32::from(index)) == 0)
            }
            Kind::Amd { extended, global } => {
                (!global || self.read(0xc000_0301) == 0)
                    && (0..self.counters).all(|index| {
                        self.read(
                            self.amd_control() + u32::from(index) * if extended { 2 } else { 1 },
                        ) == 0
                    })
            }
        }
    }

    /// Loads the exclusively reserved counter with delivery and counting stopped.
    ///
    /// # Safety
    /// Retain the probing CPU and exclusive PMU ownership acquired after a
    /// successful vacancy check. The LVTPC must be masked and NMI state ready.
    pub unsafe fn initialize(self, preload: u64) {
        match self.kind {
            Kind::IntelReference => {
                self.write(0x309 + 2, preload);
                self.write(0x390, INTEL_REF_BIT);
                self.write(0x38d, 0x9 << 8);
            }
            Kind::Amd { global, .. } => {
                self.write(self.amd_counter(), preload);
                if global {
                    self.write(0xc000_0302, 1);
                }
                self.write(self.amd_control(), 0x76 | (1 << 16) | (1 << 17) | (1 << 20));
            }
        }
    }

    /// Stops the owned producer without releasing its reserved registers.
    ///
    /// # Safety
    /// Same pinned exclusive ownership contract as `initialize`; NMI is allowed.
    pub unsafe fn stop(self) {
        match self.kind {
            Kind::IntelReference => self.write(0x38f, 0),
            Kind::Amd { global, .. } => {
                self.write(self.amd_control(), self.read(self.amd_control()) & !ENABLE);
                if global {
                    self.write(0xc000_0301, 0);
                }
            }
        }
    }

    /// Starts the reserved, initialized producer.
    ///
    /// # Safety
    /// Retain exclusive ownership and establish callback/controller readiness.
    pub unsafe fn start(self) {
        match self.kind {
            Kind::IntelReference => self.write(0x38f, INTEL_REF_BIT),
            Kind::Amd { global, .. } => {
                if global {
                    self.write(0xc000_0301, 1);
                }
                self.write(self.amd_control(), self.read(self.amd_control()) | ENABLE);
            }
        }
    }

    /// Tests only this reserved counter's overflow without clearing other events.
    ///
    /// # Safety
    /// Retain the probed CPU and counter ownership; NMI is allowed.
    pub unsafe fn has_overflow(self) -> bool {
        match self.kind {
            Kind::IntelReference => self.read(0x38e) & INTEL_REF_BIT != 0,
            Kind::Amd { global: true, .. } => self.read(0xc000_0300) & 1 != 0,
            Kind::Amd { .. } => self.read(self.amd_counter()) & (1 << (self.width - 1)) == 0,
        }
    }

    /// Clears this owned counter's status and reloads it while stopped.
    ///
    /// # Safety
    /// Same ownership as `initialize`; the producer must already be stopped.
    pub unsafe fn reload(self, preload: u64) {
        match self.kind {
            Kind::IntelReference => {
                self.write(0x30b, preload);
                self.write(0x390, INTEL_REF_BIT);
            }
            Kind::Amd { global, .. } => {
                self.write(self.amd_counter(), preload);
                if global {
                    self.write(0xc000_0302, 1);
                }
            }
        }
    }

    /// Returns reserved controls to vacant state after local delivery teardown.
    ///
    /// # Safety
    /// Producer is stopped and all local claims completed. Retain exclusive
    /// ownership until return; this does not synchronize another CPU or source.
    pub unsafe fn release(self) {
        match self.kind {
            Kind::IntelReference => {
                self.write(0x38d, 0);
                self.write(0x390, INTEL_REF_BIT);
            }
            Kind::Amd { global, .. } => {
                self.write(self.amd_control(), 0);
                if global {
                    self.write(0xc000_0302, 1);
                }
            }
        }
    }

    // Only the unsafe hardware operations above invoke these private helpers;
    // their contracts establish supported MSRs and pinned exclusive ownership.
    fn read(self, msr: u32) -> u64 {
        let low: u32;
        let high: u32;
        // SAFETY: the enclosing hardware operation validates register capability,
        // privilege and ownership; RDMSR touches only the named local register.
        unsafe {
            core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nostack, preserves_flags))
        };
        u64::from(low) | (u64::from(high) << 32)
    }

    fn write(self, msr: u32, value: u64) {
        // SAFETY: same validated MSR/CPU/ownership contract as read(). No
        // reference to ordinary memory or another CPU's register is created.
        unsafe {
            core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nostack, preserves_flags))
        };
    }
}

unittest::path_mod_test!(tests, "../ut-tests/counter.rs");
