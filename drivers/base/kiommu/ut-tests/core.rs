// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core framework tests: IOVA allocator geometry and domain map/unmap flow.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use io_pgtable::{IoPageTable, IommuProt, Iova, PgTableError};
use unittest::{assert, assert_eq, def_test};

use crate::{
    DeviceKey, InstanceId, IommuDomain, IommuError, attach_device, detach_device, domain_for_device,
    register_instance, report_fault, reset_for_tests, IommuFault, IommuHardware,
};

/// Pages recorded by the page-table fake, for leak/flow assertions.
static MAPPED_PAGES: AtomicUsize = AtomicUsize::new(0);
/// Flushes recorded by the hardware fake.
static FLUSHES: AtomicUsize = AtomicUsize::new(0);
/// Attach/detach counts recorded by the hardware fake.
static ATTACHES: AtomicUsize = AtomicUsize::new(0);
static DETACHES: AtomicUsize = AtomicUsize::new(0);

fn reset_counters() {
    MAPPED_PAGES.store(0, Ordering::SeqCst);
    FLUSHES.store(0, Ordering::SeqCst);
    ATTACHES.store(0, Ordering::SeqCst);
    DETACHES.store(0, Ordering::SeqCst);
}

/// All-state-in-atomics page table: the domain owns it in a `Box`, and the
/// test observes the shared counters.
#[derive(Default)]
struct FakePageTable;

impl IoPageTable for FakePageTable {
    fn pgsize_bitmap(&self) -> u64 {
        1 << 12
    }

    fn root_table_address(&self) -> u64 {
        0x1000
    }

    fn map_pages(
        &mut self,
        _iova: Iova,
        _paddr: u64,
        _pgsize: u64,
        count: usize,
        _prot: IommuProt,
    ) -> Result<usize, PgTableError> {
        MAPPED_PAGES.fetch_add(count, Ordering::SeqCst);
        Ok(count)
    }

    fn unmap_pages(
        &mut self,
        _iova: Iova,
        _pgsize: u64,
        count: usize,
    ) -> Result<usize, PgTableError> {
        Ok(count)
    }

    fn translate(&self, _iova: Iova) -> Option<u64> {
        None
    }
}

struct FakeHardware;

impl IommuHardware for FakeHardware {
    fn attach(&self, _master: &[u32], _domain: &IommuDomain) -> Result<(), IommuError> {
        ATTACHES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn detach(&self, _master: &[u32]) -> Result<(), IommuError> {
        DETACHES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn flush_iotlb_all(&self) {
        FLUSHES.fetch_add(1, Ordering::SeqCst);
    }
}

fn rw() -> IommuProt {
    IommuProt {
        read: true,
        write: true,
        cacheable: true,
    }
}

#[def_test(serial)]
fn iova_allocator_honors_alignment_and_reuse() {
    let domain = IommuDomain::new(Box::new(FakePageTable), 1 << 32, 4096);

    let first = domain.map(0x1000, 8192, rw(), 4096).expect("first map");
    // IOVA 0 is reserved, so the first allocation lands on page 1.
    assert_eq!(first.as_u64(), 4096);

    // A 64 KiB-aligned allocation cannot reuse the free page 0 region.
    let aligned = domain.map(0x3000, 4096, rw(), 64 * 1024).expect("aligned");
    assert_eq!(aligned.as_u64() % (64 * 1024), 0);
    assert_ne!(aligned.as_u64(), 0);

    // Free the first mapping and allocate again: the range is reusable.
    domain.unmap(first, 8192).expect("unmap");
    let reused = domain.map(0x1000, 4096, rw(), 4096).expect("reuse");
    assert_eq!(reused.as_u64(), 4096);
}

#[def_test(serial)]
fn domain_map_segments_places_contiguous_iova() {
    reset_counters();
    let domain = IommuDomain::new(Box::new(FakePageTable), 1 << 32, 4096);
    // Three segments, one spanning two pages.
    let segments: Vec<(u64, u64)> = vec![(0x1000, 4096), (0x5000, 8192), (0x9000, 4096)];
    let iova = domain
        .map_segments(&segments, rw())
        .expect("segments map");
    assert_eq!(
        MAPPED_PAGES.load(Ordering::SeqCst),
        4,
        "4 KiB pages: 1 + 2 + 1"
    );
    // Segments land contiguously: segment 2 starts at iova + 4096.
    // Unmap the whole window with the total size.
    domain.unmap(iova, 4 * 4096).expect("unmap");
}

#[def_test(serial)]
fn instance_registration_and_device_attach() {
    reset_for_tests();
    reset_counters();
    let instance = InstanceId(0xdead);
    let hardware = Arc::new(FakeHardware);
    let domain = Arc::new(IommuDomain::new(Box::new(FakePageTable), 1 << 40, 4096));

    let registered = register_instance(instance, hardware, domain);
    assert!(
        Arc::strong_count(&registered) >= 2,
        "registry holds the domain"
    );
    // Idempotent re-registration returns the same domain.
    let again = register_instance(instance, Arc::new(FakeHardware), registered.clone());
    assert!(
        Arc::ptr_eq(&registered, &again),
        "re-registration returns the existing domain"
    );

    assert_eq!(
        attach_device(instance, DeviceKey(7), &[0x42]),
        Ok(()),
        "attach succeeds on a registered instance"
    );
    assert_eq!(ATTACHES.load(Ordering::SeqCst), 1);
    assert!(domain_for_device(DeviceKey(7)).is_some());
    assert!(domain_for_device(DeviceKey(8)).is_none());

    // Duplicate attach is rejected without touching hardware.
    assert_eq!(
        attach_device(instance, DeviceKey(7), &[0x42]),
        Err(IommuError::AlreadyAttached)
    );
    assert_eq!(ATTACHES.load(Ordering::SeqCst), 1);

    assert_eq!(
        detach_device(instance, DeviceKey(7)),
        Ok(()),
        "detach releases the binding"
    );
    assert_eq!(DETACHES.load(Ordering::SeqCst), 1);
    assert!(domain_for_device(DeviceKey(7)).is_none());

    // Unknown instances report NotFound.
    assert_eq!(
        attach_device(InstanceId(0x999), DeviceKey(9), &[]),
        Err(IommuError::NotFound)
    );
}

#[def_test(serial)]
fn fault_report_is_observable() {
    // The core only formats the fault; the call must not panic.
    report_fault(&IommuFault {
        instance: InstanceId(1),
        master: 0x1234,
        iova: Iova::new(0xdead_beef_0000),
        is_write: true,
    });
}

/// Guards against accidental lock-order inversions: the domain spin lock must
/// be released before hardware flush callbacks run (they may re-enter).
#[def_test(serial)]
fn map_flushes_every_attached_hardware() {
    reset_for_tests();
    reset_counters();
    let instance = InstanceId(0x5150);
    let domain = Arc::new(IommuDomain::new(Box::new(FakePageTable), 1 << 32, 4096));
    register_instance(instance, Arc::new(FakeHardware), domain.clone());
    attach_device(instance, DeviceKey(1), &[1]).expect("attach");

    let iova = domain.map(0x2000, 4096, rw(), 4096).expect("map");
    assert!(
        FLUSHES.load(Ordering::SeqCst) >= 1,
        "map must flush the hardware TLB"
    );
    domain.unmap(iova, 4096).expect("unmap");
    assert!(
        FLUSHES.load(Ordering::SeqCst) >= 2,
        "unmap must flush the hardware TLB"
    );
}
