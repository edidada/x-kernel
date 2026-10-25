// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::alloc::Layout;

use alloc_engine::{AllocError, AllocResult};
use unittest::{assert, assert_eq, def_test};

use super::{AllocationOps, allocate, release};

const ADDRESS: usize = 0x10000;
const PAGES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Allocate,
    Map,
    Prepare,
    Release,
    Restore,
    Free,
}

#[derive(Default)]
struct MockOps {
    events: Vec<Step>,
    failures: Vec<Step>,
    owned_pages: usize,
}

impl MockOps {
    fn step(&mut self, step: Step, address: usize, pages: usize) -> AllocResult<()> {
        core::assert_eq!(address, ADDRESS);
        core::assert_eq!(pages, PAGES);
        self.events.push(step);
        if self.failures.contains(&step) {
            Err(AllocError::NoMemory)
        } else {
            Ok(())
        }
    }
}

impl AllocationOps for MockOps {
    fn allocate(&mut self, pages: usize, align: usize) -> AllocResult<usize> {
        core::assert_eq!(align, 8192);
        self.step(Step::Allocate, ADDRESS, pages)?;
        self.owned_pages = pages;
        Ok(ADDRESS)
    }

    fn map_coherent(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        self.step(Step::Map, address, pages)
    }

    fn prepare(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        self.step(Step::Prepare, address, pages)
    }

    fn release(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        self.step(Step::Release, address, pages)
    }

    fn restore(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        self.step(Step::Restore, address, pages)
    }

    fn free(&mut self, address: usize, pages: usize) {
        core::assert_eq!(address, ADDRESS);
        core::assert_eq!(pages, PAGES);
        core::assert_eq!(self.owned_pages, pages);
        self.owned_pages = 0;
        self.events.push(Step::Free);
    }
}

#[def_test]
fn zero_size_does_not_acquire_pages() {
    let mut ops = MockOps::default();
    assert_eq!(
        allocate(Layout::from_size_align(0, 1).unwrap(), &mut ops),
        Err(AllocError::InvalidInput)
    );
    assert!(ops.events.is_empty());
}

#[def_test]
fn allocation_failures_unwind_only_attempted_stages() {
    use Step::*;
    for (failure, expected) in [
        (Allocate, vec![Allocate]),
        (Map, vec![Allocate, Map, Restore, Free]),
        (
            Prepare,
            vec![Allocate, Map, Prepare, Release, Restore, Free],
        ),
    ] {
        let mut ops = MockOps {
            failures: vec![failure],
            ..MockOps::default()
        };
        assert_eq!(
            allocate(Layout::from_size_align(4097, 8192).unwrap(), &mut ops),
            Err(AllocError::NoMemory)
        );
        assert_eq!(ops.events, expected);
        assert_eq!(ops.owned_pages, 0);
    }
}

#[def_test]
fn rollback_failure_never_returns_unsafe_pages() {
    use Step::*;
    for (failures, expected) in [
        (vec![Map, Restore], vec![Allocate, Map, Restore]),
        (
            vec![Prepare, Release],
            vec![Allocate, Map, Prepare, Release],
        ),
        (
            vec![Prepare, Restore],
            vec![Allocate, Map, Prepare, Release, Restore],
        ),
    ] {
        let mut ops = MockOps {
            failures,
            ..MockOps::default()
        };
        assert_eq!(
            allocate(Layout::from_size_align(4097, 8192).unwrap(), &mut ops),
            Err(AllocError::NoMemory)
        );
        assert_eq!(ops.events, expected);
        assert_eq!(ops.owned_pages, PAGES);
    }
}

#[def_test]
fn successful_allocation_keeps_pages_until_release() {
    use Step::*;
    let mut ops = MockOps::default();
    assert_eq!(
        allocate(Layout::from_size_align(4097, 8192).unwrap(), &mut ops),
        Ok(ADDRESS)
    );
    assert_eq!(ops.events, vec![Allocate, Map, Prepare]);
    assert_eq!(ops.owned_pages, PAGES);
    assert!(release(ADDRESS, PAGES, &mut ops));
    assert_eq!(
        ops.events,
        vec![Allocate, Map, Prepare, Release, Restore, Free]
    );
    assert_eq!(ops.owned_pages, 0);
}

#[def_test]
fn ordinary_release_failure_also_retains_pages() {
    use Step::*;
    for (failure, expected) in [(Release, vec![Release]), (Restore, vec![Release, Restore])] {
        let mut ops = MockOps {
            failures: vec![failure],
            owned_pages: PAGES,
            ..MockOps::default()
        };
        assert!(!release(ADDRESS, PAGES, &mut ops));
        assert_eq!(ops.events, expected);
        assert_eq!(ops.owned_pages, PAGES);
    }
}
