// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance at streaming ownership transitions.

use alloc_engine::AllocResult;
use memaddr::VirtAddr;

use crate::{DmaDirection, DmaSyncTarget};

/// Cache contract for cacheable memory in the current unified DMA domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaCachePolicy {
    /// Cacheable direct mappings are unavailable; use coherent bounce storage.
    Unsupported,
    /// Hardware maintains CPU/device cache consistency; ordering is still required.
    Coherent,
    /// Software maintains complete cache lines exclusively owned by the mapping.
    NonCoherent {
        /// Required isolation alignment for both endpoints, a nonzero power of two.
        /// This may exceed the instruction stride to cover writeback granularity.
        alignment: usize,
    },
}

/// Operation completed at the point of coherency before the caller continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaCacheOperation {
    /// Write CPU modifications to memory for a device read.
    Clean,
    /// Discard stale CPU lines after a device write.
    Invalidate,
    /// Write CPU modifications and discard the lines before device ownership.
    CleanInvalidate,
}

/// Validated maintenance range supplied exclusively by kdma.
///
/// Private construction requires a pinned live RAM range and stopped owners before
/// the host hook runs. Host implementations can inspect it without constructing
/// arbitrary cache-maintenance requests through safe APIs.
pub struct DmaCacheRange {
    start: VirtAddr,
    len: usize,
}

impl DmaCacheRange {
    /// Return the mapped kernel address whose cache lines are exclusively owned.
    pub const fn start(&self) -> VirtAddr {
        self.start
    }

    /// Return the byte extent covered by this maintenance request.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Maintenance requests always cover at least one byte.
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Host cache maintenance for the single DMA domain.
///
/// Hooks must complete synchronously without sleeping and support IRQ context.
/// `Unsupported` selects bounce storage for cacheable buffers. Uncached buffers
/// need only ordering. This interface describes software maintenance capability,
/// independently of any future device-specific coherency declaration.
#[kiface::interface]
pub trait DmaCacheIf {
    /// Return the policy supported by this host for cacheable buffers.
    fn policy() -> DmaCachePolicy;

    /// Complete maintenance for an exclusively owned, whole-cache-line range.
    ///
    /// # Errors
    /// Returns an error if the operation cannot complete. A failed transition
    /// retains the old owner and must be retried before access or release.
    ///
    /// The range is initialized pinned RAM with stable attributes and both
    /// owners stopped. Implementations validate their hardware alignment before
    /// executing the operation; the borrowed request cannot outlive the stopped-owner scope.
    fn maintain(range: &DmaCacheRange, operation: DmaCacheOperation) -> AllocResult<()>;
}

/// # Safety
/// The nonempty range is pinned initialized RAM with stable attributes and
/// exclusive complete cache lines; both owners are stopped until return.
pub(crate) unsafe fn maintain(
    start: VirtAddr,
    len: usize,
    operation: DmaCacheOperation,
) -> AllocResult<()> {
    DmaCacheIf::maintain(&DmaCacheRange { start, len }, operation)
}

/// # Safety
/// The original mapping remains live over exclusive complete allocated pages;
/// hardware and all CPU aliases have stopped until attributes/share state change.
pub(crate) unsafe fn prepare_attribute_change(start: VirtAddr, len: usize) -> AllocResult<()> {
    if DmaCacheIf::policy() != DmaCachePolicy::Unsupported {
        // SAFETY: caller retains whole live pages and their current attributes
        // with both owners stopped until this synchronous clean/invalidate returns.
        unsafe { maintain(start, len, DmaCacheOperation::CleanInvalidate) }?;
    }
    Ok(())
}

pub(crate) fn operation(
    direction: DmaDirection,
    target: DmaSyncTarget,
) -> Option<DmaCacheOperation> {
    match (direction, target) {
        (DmaDirection::DriverToDevice, DmaSyncTarget::Device) => Some(DmaCacheOperation::Clean),
        (_, DmaSyncTarget::Device) => Some(DmaCacheOperation::CleanInvalidate),
        (DmaDirection::DriverToDevice, DmaSyncTarget::Cpu) => None,
        (_, DmaSyncTarget::Cpu) => Some(DmaCacheOperation::Invalidate),
    }
}

pub(crate) fn can_direct(policy: DmaCachePolicy, start: usize, len: usize) -> bool {
    match policy {
        DmaCachePolicy::Unsupported => false,
        DmaCachePolicy::Coherent => true,
        DmaCachePolicy::NonCoherent { alignment } => {
            alignment.is_power_of_two()
                && start.is_multiple_of(alignment)
                && len.is_multiple_of(alignment)
        }
    }
}
