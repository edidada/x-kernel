// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File mapping callback implementation.
use alloc::sync::Arc;

use kerrno::KResult;
use kfs::{CachedFile, File};
use khal::paging::PageSize;
use ksync::Mutex;
use kvfs::MmapMapper;
use memaddr::{MemoryAddr, PhysAddrRange, VirtAddr};
use memspace::{AddrSpace, backend::Backend};

use crate::{new_cow, new_file};

pub struct FileMapper {
    start: VirtAddr,
    pub length: usize,
    offset: usize,
    page_size: PageSize,
    shared: bool,
    file: Arc<File>,
    aspace: Arc<Mutex<AddrSpace>>,
    backend: Option<Backend>,
}

impl FileMapper {
    pub fn new(
        start: VirtAddr,
        length: usize,
        offset: usize,
        page_size: PageSize,
        shared: bool,
        file: Arc<File>,
        aspace: Arc<Mutex<AddrSpace>>,
    ) -> Self {
        Self {
            start,
            length,
            offset,
            page_size,
            shared,
            file,
            aspace,
            backend: None,
        }
    }

    pub fn into_backend(self) -> KResult<Backend> {
        self.backend.ok_or(kerrno::KError::NoSuchDevice)
    }
}

impl MmapMapper for FileMapper {
    fn offset(&self) -> usize {
        self.offset
    }

    fn map_physical(&mut self, mut range: PhysAddrRange) -> kvfs::VfsResult<()> {
        // `offset` is a userspace mmap argument. Adding it to the device PA
        // must not panic: a page-aligned but huge offset can overflow usize.
        let mapped_start = range
            .start
            .checked_add(self.offset)
            .ok_or(kvfs::VfsError::InvalidInput)?;
        if mapped_start >= range.end {
            return Err(kvfs::VfsError::InvalidInput);
        }
        range.start = mapped_start;
        self.length = self.length.min(range.size().align_down(self.page_size));
        // Store VA−PA as a wrapping `usize` bit pattern. `as isize`
        // subtraction overflows when the physical start is above `isize::MAX`.
        self.backend = Some(Backend::new_linear(
            self.start.as_usize().wrapping_sub(range.start.as_usize()) as isize,
        ));
        Ok(())
    }

    fn map_file_backed(&mut self) -> kvfs::VfsResult<()> {
        let backend = if self.shared {
            let cache = CachedFile::get_or_create(self.file.location().clone())?;
            new_file(
                self.start,
                cache,
                self.file.flags(),
                self.offset,
                &self.aspace,
            )
        } else {
            let file_backend = (*self.file).backend()?.clone();
            let file_start = self.offset as u64;
            let file_end = file_start
                .checked_add(self.length as u64)
                .ok_or(kvfs::VfsError::InvalidInput)?;
            new_cow(
                self.start,
                self.page_size,
                file_backend,
                file_start,
                Some(file_end),
            )
        };
        self.backend = Some(backend);
        Ok(())
    }
}
