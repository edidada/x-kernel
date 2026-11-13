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

    // A non-zero atime bit without the group in `attr_clr` never reaches this
    // function: `check_mount_attr_atime` rejects it as `build_mount_kattr`
    // does, so every case below carries the group in `attr_clr`.

    // `MOUNT_ATTR_RELATIME` is zero, so "request relatime" is exactly
    // `attr_clr = MOUNT_ATTR__ATIME, attr_set = 0` -- the form systemd uses,
    // and the one an earlier revision turned into strictatime by clearing
    // both flags and setting neither.
    let (set, clr) = mount_attr_set_clr(0, ATIME);
    assert!(clr.contains(MountFlags::NOATIME));
    assert!(clr.contains(MountFlags::RELATIME));
    assert!(set.contains(MountFlags::RELATIME));

    // NOATIME replaces the whole policy too: the atime group is an enum.
    let (set, clr) = mount_attr_set_clr(NOATIME, ATIME);
    assert!(set.contains(MountFlags::NOATIME));
    assert!(!set.contains(MountFlags::RELATIME));
    assert!(clr.contains(MountFlags::RELATIME));

    // A strictatime request clears both and sets neither.
    let (set, clr) = mount_attr_set_clr(STRICTATIME, ATIME);
    assert!(clr.contains(MountFlags::NOATIME));
    assert!(clr.contains(MountFlags::RELATIME));
    assert!(!set.contains(MountFlags::RELATIME));
    assert!(!set.contains(MountFlags::NOATIME));

    // NODIRATIME is independent of the atime group and passes straight through.
    let (set, _) = mount_attr_set_clr(NODIRATIME, 0);
    assert!(set.contains(MountFlags::NODIRATIME));
}

#[def_test]
fn an_atime_request_outside_the_cleared_group_is_rejected() {
    use super::check_mount_attr_atime;

    const ATIME: u64 = 0x70;
    const NOATIME: u64 = 0x10;
    const STRICTATIME: u64 = 0x20;

    // The ABI carries the atime value in `attr_set` only together with the
    // group in `attr_clr`; `build_mount_kattr` answers EINVAL otherwise.
    assert!(check_mount_attr_atime(NOATIME, ATIME).is_ok());
    assert!(check_mount_attr_atime(STRICTATIME, ATIME).is_ok());
    assert!(check_mount_attr_atime(0, ATIME).is_ok());
    assert!(check_mount_attr_atime(0, 0).is_ok());
    assert_eq!(
        check_mount_attr_atime(NOATIME, 0),
        Err(kerrno::KError::InvalidInput)
    );
    assert_eq!(
        check_mount_attr_atime(STRICTATIME, 0),
        Err(kerrno::KError::InvalidInput)
    );
    // A partially cleared group is not a policy either.
    assert_eq!(
        check_mount_attr_atime(0, NOATIME),
        Err(kerrno::KError::InvalidInput)
    );
}
