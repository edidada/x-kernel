// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU hardware entropy for seeding the TEE CSPRNG.
//!
//! Timer ticks are attacker-predictable and must not be the sole seed for
//! attestation nonces or key material.

/// Fill `buf` from a CPU RNG instruction if the feature is present.
///
/// Returns `false` when the architecture or CPU does not provide a usable
/// hardware RNG. On failure `buf` is left unchanged.
pub(super) fn fill_cpu_entropy(buf: &mut [u8; 32]) -> bool {
    let mut tmp = [0u8; 32];
    if !fill_cpu_entropy_inner(&mut tmp) {
        return false;
    }
    *buf = tmp;
    true
}

fn fill_cpu_entropy_inner(buf: &mut [u8]) -> bool {
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            x86_64::fill(buf)
        } else if #[cfg(target_arch = "aarch64")] {
            aarch64::fill(buf)
        } else {
            let _ = buf;
            false
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    const RDRAND_RETRIES: u32 = 10;
    const RDSEED_RETRIES: u32 = 100;

    pub(super) fn fill(buf: &mut [u8]) -> bool {
        if has_rdseed() && fill_with(rdseed64, buf) {
            return true;
        }
        has_rdrand() && fill_with(rdrand64, buf)
    }

    fn has_rdrand() -> bool {
        // SAFETY: CPUID leaf 1 is architecturally defined and does not access memory.
        let leaf = unsafe { core::arch::x86_64::__cpuid(1) };
        leaf.ecx & (1 << 30) != 0
    }

    fn has_rdseed() -> bool {
        // SAFETY: CPUID leaf 0 only reports the maximum leaf and does not access memory.
        let max_leaf = unsafe { core::arch::x86_64::__cpuid(0) }.eax;
        if max_leaf < 7 {
            return false;
        }
        // SAFETY: leaf 7 subleaf 0 is valid because the maximum leaf is at least 7.
        let leaf = unsafe { core::arch::x86_64::__cpuid_count(7, 0) };
        leaf.ebx & (1 << 18) != 0
    }

    fn rdrand64() -> Option<u64> {
        let mut val = 0u64;
        for _ in 0..RDRAND_RETRIES {
            // SAFETY: RDRAND was advertised by CPUID; `val` is a writable stack u64.
            if unsafe { core::arch::x86_64::_rdrand64_step(&mut val) } == 1 {
                return Some(val);
            }
        }
        None
    }

    fn rdseed64() -> Option<u64> {
        let mut val = 0u64;
        for _ in 0..RDSEED_RETRIES {
            // SAFETY: RDSEED was advertised by CPUID; `val` is a writable stack u64.
            if unsafe { core::arch::x86_64::_rdseed64_step(&mut val) } == 1 {
                return Some(val);
            }
        }
        None
    }

    fn fill_with(next_u64: fn() -> Option<u64>, buf: &mut [u8]) -> bool {
        let mut offset = 0;
        while offset < buf.len() {
            let Some(word) = next_u64() else {
                return false;
            };
            let bytes = word.to_le_bytes();
            let n = (buf.len() - offset).min(bytes.len());
            buf[offset..offset + n].copy_from_slice(&bytes[..n]);
            offset += n;
        }
        true
    }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    pub(super) fn fill(buf: &mut [u8]) -> bool {
        if !has_rndr() {
            return false;
        }
        let mut offset = 0;
        while offset < buf.len() {
            let Some(word) = rndr64() else {
                return false;
            };
            let bytes = word.to_le_bytes();
            let n = (buf.len() - offset).min(bytes.len());
            buf[offset..offset + n].copy_from_slice(&bytes[..n]);
            offset += n;
        }
        true
    }

    fn has_rndr() -> bool {
        let isar0: u64;
        // SAFETY: ID_AA64ISAR0_EL1 is a read-only identification register.
        unsafe {
            core::arch::asm!(
                "mrs {isar0}, ID_AA64ISAR0_EL1",
                isar0 = out(reg) isar0,
                options(nomem, nostack, preserves_flags)
            );
        }
        (isar0 >> 60) & 0xf != 0
    }

    fn rndr64() -> Option<u64> {
        let val: u64;
        let ok: u64;
        // SAFETY: RNDR is present per ID_AA64ISAR0_EL1; a failed read sets Z and
        // does not write an architectural random value we treat as entropy.
        unsafe {
            core::arch::asm!(
                "mrs {val}, S3_3_C2_C4_0",
                "cset {ok}, ne",
                val = out(reg) val,
                ok = out(reg) ok,
                options(nomem, nostack)
            );
        }
        (ok != 0).then_some(val)
    }
}
