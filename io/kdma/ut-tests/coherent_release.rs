// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use unittest::def_test;

use super::{AllocError, DmaMappingCompletion, release_after_restore};

#[def_test]
fn page_release_requires_successful_normal_mapping() {
    let releases = Cell::new(0);
    let release = || releases.set(releases.get() + 1);
    assert!(release_after_restore(Err(AllocError::NoMemory), release).is_err());
    assert_eq!(releases.get(), 0);
    assert!(release_after_restore(Ok(()), release).is_ok());
    assert_eq!(releases.get(), 1);
}

#[cfg(feature = "dma-trace")]
#[def_test]
fn failed_restore_preserves_the_coherent_allocation_trace() {
    use core::ptr::NonNull;

    use super::{DmaAllocator, DmaBusAddress, DmaInfo};

    // A synthetic identity for the trace ledger; it is never dereferenced.
    let dma = DmaInfo {
        cpu_addr: NonNull::dangling(),
        bus_addr: DmaBusAddress::new(4096),
    };
    let mut allocator = DmaAllocator::new();
    allocator.trace_coherent_alloc(dma, 1);
    assert!(allocator.validate_coherent_free(dma, 1));
    assert!(
        release_after_restore(Err(AllocError::NoMemory), || {
            allocator.trace_coherent_free(dma);
        })
        .is_err()
    );
    assert!(allocator.validate_coherent_free(dma, 1));
    assert!(
        release_after_restore(Ok(()), || {
            allocator.trace_coherent_free(dma);
        })
        .is_ok()
    );
    assert!(allocator.coherent_allocs.is_empty());
}

#[def_test]
fn coherent_allocation_requires_positive_confirmation() {
    use super::confirm_mapping;

    assert!(confirm_mapping(Ok(None)).is_ok());
    assert!(confirm_mapping(Ok(Some(DmaMappingCompletion::new(|| true)))).is_ok());
    assert!(confirm_mapping(Ok(Some(DmaMappingCompletion::new(|| false)))).is_err());
    assert!(confirm_mapping(Err(AllocError::NoMemory)).is_err());
}
