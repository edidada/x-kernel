// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn register_pool_and_recycle_mixed_size_buffers() {
    let mut backing = alloc::vec![0u8; 65536];
    let cpu_base = NonNull::new(backing.as_mut_ptr()).unwrap();
    let bus_base = DmaBusAddress::new(0x1000_0000);
    let mut pool = BouncePool::new();
    assert!(!pool.is_initialized());
    assert!(matches!(pool.allocate(64), Err(AllocError::NotInitialized)));
    assert!(!pool.is_initialized());
    assert_eq!(pool.test_used_total_bytes(), (0, 0));
    pool.initialize(
        DmaInfo {
            cpu_addr: cpu_base,
            bus_addr: bus_base,
        },
        backing.len(),
    );
    assert!(pool.is_initialized());

    for _ in 0..8 {
        let mut allocations = alloc::vec::Vec::new();
        for (index, len) in [64, 4096, 8193].into_iter().enumerate() {
            let (dma, mut mapping) = pool.allocate(len).unwrap();
            let offset = dma.cpu_addr.as_ptr() as usize - cpu_base.as_ptr() as usize;
            assert!(offset + len <= backing.len());
            assert_eq!(dma.bus_addr.as_u64(), bus_base.as_u64() + offset as u64);
            let pattern = index as u8 + 1;
            mapping.copy_from_slice(&alloc::vec![MaybeUninit::new(pattern); len]);
            allocations.push((dma, mapping, pattern));
        }
        for (dma, mapping, pattern) in allocations {
            // SAFETY: this token retains exclusive live storage; all bytes were
            // initialized by copy_from_slice. Simulate a completed device write.
            let bounce =
                unsafe { core::slice::from_raw_parts_mut(dma.cpu_addr.as_ptr(), mapping.len()) };
            assert!(bounce.iter().all(|byte| *byte == pattern));
            bounce.fill(pattern + 16);
            let mut received = alloc::vec![MaybeUninit::uninit(); mapping.len()];
            mapping.sync_for_cpu(Some(&mut received));
            assert!(received.iter().all(|byte| {
                // SAFETY: the simulated device initialized every bounce byte and
                // sync_for_cpu copied the complete matching extent above.
                unsafe { byte.assume_init() == pattern + 16 }
            }));
            pool.deallocate(mapping);
        }
    }
}
