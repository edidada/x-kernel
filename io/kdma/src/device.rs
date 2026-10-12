// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device address limits, independent of shared DMA allocation state.

use alloc_engine::{AllocError, AllocResult};

/// Immutable address capabilities selected by device probe.
///
/// All contexts currently use the platform address conversion and the shared
/// uncached bounce backend. A restrictive mask can reject an allocation; it
/// does not request a private pool or guarantee low-address memory availability.
#[derive(Debug)]
pub struct DmaDeviceContext {
    streaming_mask: u64,
    coherent_mask: u64,
}

impl DmaDeviceContext {
    /// Declare streaming and coherent masks consisting of contiguous low bits.
    ///
    /// # Errors
    /// Returns `InvalidInput` for a zero or non-contiguous mask.
    pub fn new(streaming_mask: u64, coherent_mask: u64) -> AllocResult<Self> {
        for mask in [streaming_mask, coherent_mask] {
            if mask == 0 || (mask != u64::MAX && !(mask + 1).is_power_of_two()) {
                return Err(AllocError::InvalidInput);
            }
        }
        Ok(Self {
            streaming_mask,
            coherent_mask,
        })
    }

    /// Preserve the existing platform backend's unrestricted address policy.
    /// Probe code should use `new` when the device has narrower address limits.
    pub const fn platform_default() -> Self {
        Self {
            streaming_mask: u64::MAX,
            coherent_mask: u64::MAX,
        }
    }

    pub(crate) fn check_range(
        &self,
        start: u64,
        len: usize,
        is_streaming: bool,
    ) -> AllocResult<()> {
        let end = start
            .checked_add(len.checked_sub(1).ok_or(AllocError::InvalidInput)? as u64)
            .ok_or(AllocError::InvalidInput)?;
        let mask = if is_streaming {
            self.streaming_mask
        } else {
            self.coherent_mask
        };
        if end > mask {
            return Err(AllocError::InvalidInput);
        }
        Ok(())
    }
}
