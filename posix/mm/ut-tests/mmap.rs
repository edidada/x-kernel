// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{
    MAP_POPULATE, MAP_PRIVATE, MAP_SHARED, MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE,
    PROT_READ,
};
use memaddr::PAGE_SIZE_4K;
use memfs::shmem::create_memfd_file;
use posix_types::UserConstPtr;
use unittest::def_test;

use super::{
    MADV_DONTNEED, MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED, MadviseRequest,
    MremapFlags, sys_mmap, sys_munmap,
};

#[def_test]
fn mremap_dontunmap_requires_equal_sizes() {
    let flags = MremapFlags::from_raw(MREMAP_MAYMOVE | MREMAP_DONTUNMAP).unwrap();

    assert!(flags.validate_args(0x2000, 0x3000).is_err());
    assert!(flags.validate_args(0x2000, 0x2000).is_ok());
}

#[def_test]
fn mremap_fixed_and_dontunmap_require_maymove() {
    assert!(
        MremapFlags::from_raw(MREMAP_FIXED)
            .unwrap()
            .validate_args(0x2000, 0x2000)
            .is_err()
    );
    assert!(
        MremapFlags::from_raw(MREMAP_DONTUNMAP)
            .unwrap()
            .validate_args(0x2000, 0x2000)
            .is_err()
    );
}

/// Linux `do_madvise()` validates the range before acting on the advice, so an
/// unaligned address or an overflowing range is rejected even for the pure
/// performance hints that this kernel honours as no-ops.
#[def_test]
fn madvise_validates_the_range_before_the_advice() {
    let unaligned = 0x1000 + 1;
    // Page-aligned, and `addr + 0x2000` still overflows.
    let overflowing = usize::MAX - 0xFFF;
    // Page-aligned; `checked_add` survives, but rounding the end up to a
    // page boundary wraps.
    let rounding_overflows = 0xFFFF_FFFF_FFFF_F000;

    for advice in [MADV_NORMAL, MADV_RANDOM, MADV_WILLNEED].map(|advice| advice as i32) {
        assert!(MadviseRequest::from_raw(unaligned, 0x1000, advice).is_err());
        assert!(MadviseRequest::from_raw(overflowing, 0x2000, advice).is_err());
        assert!(
            MadviseRequest::from_raw(rounding_overflows, 0xFFF, advice).is_err(),
            "end rounding past the address space must be EINVAL"
        );
    }

    // Well-formed requests carry the range; only DONTNEED discards, the
    // hints still need a fully mapped range (`ENOMEM` at the aspace layer).
    for advice in [MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED] {
        let request = MadviseRequest::from_raw(0x2000, 0x1000, advice as i32).unwrap();
        assert!(request.is_some());
        assert!(!request.unwrap().is_discard);
    }
    assert!(matches!(
        MadviseRequest::from_raw(0x2000, 0x1000, MADV_DONTNEED as i32),
        Ok(Some(_))
    ));
}

#[def_test(user)]
fn mmap_populate_succeeds_when_prefault_reaches_past_eof() {
    let cred = kprocess::current_cred();
    let file = create_memfd_file("mmap-populate-eof", false, cred.clone())
        .expect("create memfd")
        .into_file(cred)
        .expect("open memfd");
    file.truncate(PAGE_SIZE_4K as u64)
        .expect("size memfd to one page");
    let marker = 0x5a;
    assert_eq!(file.write(&[marker]).expect("seed first file page"), 1);
    let files = kprocess::current_files();
    let fd = files.add_file(file, false).expect("install memfd");

    // The second page lies beyond EOF, so prefaulting cannot finish.
    let mapping_bytes = 2 * PAGE_SIZE_4K;
    for map_type in [MAP_SHARED, MAP_PRIVATE] {
        let address = sys_mmap(0, mapping_bytes, PROT_READ, map_type | MAP_POPULATE, fd, 0)
            .expect("MAP_POPULATE must succeed despite EOF") as usize;
        let first_byte = UserConstPtr::<u8>::from(address).read_vm();
        sys_munmap(address, mapping_bytes).expect("unmap returned range");
        assert_eq!(first_byte.expect("read backed page"), marker);
    }
    files.close_file(fd).expect("close memfd");
}
