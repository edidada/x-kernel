// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RNG abstraction — ChaCha20-based cryptographic random number generator.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
pub use rand_core::{Infallible, Rng, TryCryptoRng, TryRng};

/// Trait for cryptographic random number generation.
pub trait CryptoRng: rand_core::CryptoRng {}

impl<T> CryptoRng for T where T: rand_core::CryptoRng + ?Sized {}

/// Production ChaCha20 CSPRNG that can only be constructed from a 256-bit seed.
///
/// The seed must come from a hardware RNG or another cryptographic entropy
/// source. This type does not accept a 64-bit integer seed.
pub struct ChaCha20Csprng {
    inner: ChaCha20Rng,
}

impl ChaCha20Csprng {
    /// Create a CSPRNG from a 256-bit seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            inner: ChaCha20Rng::from_seed(seed),
        }
    }

    fn from_chacha(inner: ChaCha20Rng) -> Self {
        Self { inner }
    }
}

impl TryRng for ChaCha20Csprng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> core::result::Result<u32, Self::Error> {
        Ok(self.inner.next_u32())
    }

    fn try_next_u64(&mut self) -> core::result::Result<u64, Self::Error> {
        Ok(self.inner.next_u64())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), Self::Error> {
        self.inner.fill_bytes(dest);
        Ok(())
    }
}

impl TryCryptoRng for ChaCha20Csprng {}

/// ChaCha20-based deterministic RNG for tests and reproducible validation.
///
/// `seed_from_u64` has only 64 bits of seed material and must not be used as a
/// production entropy source. Production code should construct
/// [`ChaCha20Csprng`] from a 256-bit hardware or OS seed.
pub struct DeterministicRng {
    inner: ChaCha20Csprng,
}

impl DeterministicRng {
    /// Create a new RNG seeded from a u64 value.
    ///
    /// Intended for tests. Do not seed production key material or attestation
    /// nonces from a 64-bit integer (including timestamps or TSC ticks).
    pub fn seed_from_u64(seed: u64) -> Self {
        Self {
            inner: ChaCha20Csprng::from_chacha(ChaCha20Rng::seed_from_u64(seed)),
        }
    }

    /// Create a new RNG seeded from a 32-byte seed.
    pub fn seed_from_bytes(seed: &[u8; 32]) -> Self {
        Self {
            inner: ChaCha20Csprng::from_seed(*seed),
        }
    }
}

impl TryRng for DeterministicRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> core::result::Result<u32, Self::Error> {
        self.inner.try_next_u32()
    }

    fn try_next_u64(&mut self) -> core::result::Result<u64, Self::Error> {
        self.inner.try_next_u64()
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), Self::Error> {
        self.inner.try_fill_bytes(dest)
    }
}

impl TryCryptoRng for DeterministicRng {}

pub(crate) struct RngAdapter<'a> {
    inner: &'a mut dyn CryptoRng,
}

impl<'a> RngAdapter<'a> {
    pub(crate) fn new(inner: &'a mut dyn CryptoRng) -> Self {
        Self { inner }
    }
}

impl TryRng for RngAdapter<'_> {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> core::result::Result<u32, Self::Error> {
        Ok(self.inner.next_u32())
    }

    fn try_next_u64(&mut self) -> core::result::Result<u64, Self::Error> {
        Ok(self.inner.next_u64())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), Self::Error> {
        self.inner.fill_bytes(dest);
        Ok(())
    }
}

impl TryCryptoRng for RngAdapter<'_> {}
