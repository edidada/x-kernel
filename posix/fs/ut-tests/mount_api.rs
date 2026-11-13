// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input validation of `mount_setattr(2)`, checked against `fs/namespace.c`.

use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

use super::{
    MOUNT_ATTR_SIZE_VER0, MOUNT_ATTR_VALID_FLAGS, check_mount_attr_bits, mount_attr_set_clr,
};

#[def_test]
fn every_mount_attr_bit_linux_accepts_is_accepted() {
    // `MOUNT_SETATTR_VALID_FLAGS` is FSMOUNT_VALID_FLAGS (RDONLY|NOSUID|NODEV|
    // NOEXEC|__ATIME|NODIRATIME|NOSYMFOLLOW) plus MOUNT_ATTR_IDMAP.
    const RDONLY: u64 = 0x1;
    const NOSUID: u64 = 0x2;
    const NODEV: u64 = 0x4;
    const NOEXEC: u64 = 0x8;
    const ATIME: u64 = 0x70;
    const NODIRATIME: u64 = 0x80;
    const IDMAP: u64 = 0x10_0000;
    const NOSYMFOLLOW: u64 = 0x20_0000;
    let all = RDONLY | NOSUID | NODEV | NOEXEC | ATIME | NODIRATIME | IDMAP | NOSYMFOLLOW;
    assert_eq!(MOUNT_ATTR_VALID_FLAGS, all, "the mask is Linux's");
    assert!(check_mount_attr_bits(all).is_ok());
    assert!(check_mount_attr_bits(0).is_ok());

    // 0x1000 is not a MOUNT_ATTR_* bit. An earlier revision included it, which
    // made `mount_setattr` accept a request Linux answers with EINVAL.
    assert_eq!(check_mount_attr_bits(0x1000), Err(KError::InvalidInput));
    assert_eq!(
        check_mount_attr_bits(RDONLY | 0x1000),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        check_mount_attr_bits(0x4000_0000),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn the_published_mount_attr_size_is_32() {
    // `MOUNT_ATTR_SIZE_VER0` is `sizeof(struct mount_attr)` in Linux, and the
    // syscall rejects anything smaller.
    assert_eq!(MOUNT_ATTR_SIZE_VER0, 32);
}

#[def_test]
fn an_atime_request_replaces_the_whole_policy() {
    use kvfs::MountFlags;

    const ATIME: u64 = 0x70;
    const NOATIME: u64 = 0x10;
    const STRICTATIME: u64 = 0x20;
    const NODIRATIME: u64 = 0x80;

    // Any atime bit clears the existing selection first, so a request cannot
    // end up with both relatime and noatime set.
    let (set, clr) = mount_attr_set_clr(NOATIME, 0);
    assert!(set.contains(MountFlags::NOATIME));
    assert!(clr.contains(MountFlags::RELATIME));

    let (set, clr) = mount_attr_set_clr(STRICTATIME, 0);
    assert!(!set.contains(MountFlags::NOATIME));
    assert!(clr.contains(MountFlags::RELATIME));

    // An explicit clear of the atime group drops both selections.
    let (_, clr) = mount_attr_set_clr(0, ATIME);
    assert!(clr.contains(MountFlags::NOATIME));
    assert!(clr.contains(MountFlags::RELATIME));

    // NODIRATIME is independent of the atime group and passes straight through.
    let (set, _) = mount_attr_set_clr(NODIRATIME, 0);
    assert!(set.contains(MountFlags::NODIRATIME));
}
