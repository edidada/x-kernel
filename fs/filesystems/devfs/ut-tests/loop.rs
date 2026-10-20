// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn ioctl_permissions_follow_command_and_open_mode() {
    use linux_raw_sys::ioctl::{BLKFRASET, BLKRASET};
    use subsys_block::BlockOpenMode as Mode;

    let disk = alloc::sync::Arc::new(
        subsys_block::Gendisk::new(
            "test-loop-ioctl".into(),
            240,
            0,
            1,
            Box::new(Arc::new(LoopDevice::new(0))),
        )
        .expect("valid loop disk"),
    );
    let device = subsys_block::add_disk(disk.clone()).expect("publish loop ioctl test disk");
    for (cmd, mode, has_sys_admin, error) in [
        (
            LOOP_SET_STATUS,
            Mode::READ,
            false,
            KError::from(LinuxError::EPERM),
        ),
        (LOOP_SET_STATUS, Mode::WRITE, false, KError::BadAddress),
        (LOOP_SET_STATUS, Mode::READ, true, KError::BadAddress),
        (LOOP_SET_FD, Mode::READ, false, KError::BadFileDescriptor),
        (
            LOOP_CLR_FD,
            Mode::READ,
            false,
            KError::from(LinuxError::ENXIO),
        ),
        (BLKRASET, Mode::WRITE, false, KError::PermissionDenied),
        (BLKFRASET, Mode::WRITE, false, KError::PermissionDenied),
    ] {
        assert_eq!(
            device.as_ref().ioctl(
                mode,
                cmd,
                usize::MAX,
                has_sys_admin,
                || panic!("these commands do not copy a generic int"),
                |_| panic!("these commands do not copy generic output"),
            ),
            Err(error)
        );
    }
    subsys_block::del_gendisk(disk.device_number()).expect("remove loop ioctl test disk");
}
