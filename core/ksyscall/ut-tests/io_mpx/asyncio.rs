// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kasyncio::{
    abi::{
        ASYNCIO_ENTER_TIMEOUT, ASYNCIO_OP_READ, ASYNCIO_PARAMS_VERSION, AsyncioParams, AsyncCqe,
        AsyncSqe, header,
    },
    AsyncioSetup,
};
use kerrno::KError;
use posix_types::{UserConstPtr, UserPtr};
use unittest::def_test;
use unittest_support::TestUserValue;

use super::*;

fn setup_context(entries: u32) -> (i32, AsyncioSetup) {
    let params = TestUserValue::<AsyncioParams>::from_value(AsyncioParams {
        version: ASYNCIO_PARAMS_VERSION,
        ..AsyncioParams::default()
    })
    .expect("params user value");
    let fd = sys_asyncio_setup(entries, UserPtr::from(params.as_user_ptr() as usize))
        .expect("setup must succeed") as i32;
    let value = params.read();
    assert_eq!(value.sq_entries, entries);
    assert_eq!(value.cq_entries, entries);
    assert_ne!(value.ring_user_va, 0);
    assert_ne!(value.ring_user_len, 0);
    (
        fd,
        AsyncioSetup {
            file: setup_file(fd),
            ring_user_va: value.ring_user_va,
            ring_user_len: value.ring_user_len,
            sq_entries: value.sq_entries,
            cq_entries: value.cq_entries,
        },
    )
}

fn setup_file(fd: i32) -> alloc::sync::Arc<kvfs::VfsFile> {
    kprocess::current_files().get_file(fd).expect("setup fd")
}

fn write_field(base: usize, offset: usize, value: u32) {
    UserPtr::<u32>::from(base + offset)
        .write_vm(value)
        .expect("write ring field");
}

fn read_field(base: usize, offset: usize) -> u32 {
    UserPtr::<u32>::from(base + offset)
        .read_vm()
        .expect("read ring field")
}

#[def_test(user, serial)]
fn setup_enter_publishes_the_submitted_completion() {
    let (fd, setup) = setup_context(8);
    let base = setup.ring_user_va;
    let sq_entries = setup.sq_entries;
    drop(setup);

    let sqe = AsyncSqe {
        opcode: ASYNCIO_OP_READ,
        cookie: 0x5A5A_1234,
        ..AsyncSqe::default()
    };
    UserPtr::from(base + header::LEN)
        .write_vm(sqe)
        .expect("write SQE");
    write_field(base, header::SQ_TAIL, 1);

    let submitted = sys_asyncio_enter(
        fd,
        1,
        1,
        kasyncio::abi::ASYNCIO_ENTER_WAIT,
        UserConstPtr::default(),
    )
    .expect("enter must succeed");
    assert_eq!(submitted, 1);
    assert_eq!(read_field(base, header::SQ_HEAD), 1);
    assert_eq!(read_field(base, header::CQ_TAIL), 1);

    let cqe_offset = header::LEN + sq_entries as usize * kasyncio::abi::ASYNCIO_SQE_SIZE;
    let cqe: AsyncCqe = UserPtr::from(base + cqe_offset)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.cookie, 0x5A5A_1234);
    assert_eq!(cqe.res, 0);

    // Consume the CQE, then close the last file reference: release must
    // publish the shutdown flag but leave the user mapping valid.
    write_field(base, header::CQ_HEAD, 1);
    posix_fs::sys_close(fd).expect("close");
    assert_ne!(
        read_field(base, header::CTX_FLAGS) & header::CTX_FLAG_SHUTDOWN,
        0
    );
}

#[def_test(user, serial)]
fn setup_and_enter_reject_invalid_input() {
    let mut params = TestUserValue::<AsyncioParams>::from_value(AsyncioParams {
        version: ASYNCIO_PARAMS_VERSION,
        ..AsyncioParams::default()
    })
    .expect("params user value");
    let ptr = UserPtr::from(params.as_user_ptr() as usize);

    assert_eq!(
        sys_asyncio_setup(3, ptr),
        Err(KError::InvalidInput),
        "non-power-of-two entry counts are rejected"
    );
    assert_eq!(
        sys_asyncio_setup(0, ptr),
        Err(KError::InvalidInput),
        "zero entry counts are rejected"
    );

    let mut bad_version = AsyncioParams {
        version: ASYNCIO_PARAMS_VERSION + 1,
        ..AsyncioParams::default()
    };
    params.write(bad_version);
    assert_eq!(
        sys_asyncio_setup(8, ptr),
        Err(KError::InvalidInput),
        "unknown parameter versions are rejected"
    );
    bad_version.version = ASYNCIO_PARAMS_VERSION;
    params.write(bad_version);

    let (fd, setup) = setup_context(8);
    let ring_base = setup.ring_user_va;
    drop(setup);
    assert_eq!(
        sys_asyncio_enter(fd, 0, 0, 1 << 7, UserConstPtr::default()),
        Err(KError::InvalidInput),
        "unknown enter flags are rejected"
    );
    assert_eq!(
        sys_asyncio_enter(fd, 0, 0, ASYNCIO_ENTER_TIMEOUT, UserConstPtr::default()),
        Err(KError::InvalidInput),
        "a timeout without WAIT is rejected"
    );

    write_field(ring_base, header::SQ_TAIL, 1);
    assert_eq!(
        sys_asyncio_enter(fd, 1, 0, 0, UserConstPtr::default()),
        Err(KError::InvalidInput),
        "submitting more SQEs than the user published is rejected"
    );

    posix_fs::sys_close(fd).expect("close");
}
