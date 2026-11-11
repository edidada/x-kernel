// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kasyncio::abi::{
    IORING_ENTER_GETEVENTS, IORING_FEAT_SINGLE_MMAP, IORING_OFF_SQES, IORING_OFF_SQ_RING,
    IORING_OP_NOP, IoUringCqe, IoUringParams, IoUringSqe,
};
use kerrno::KError;
use linux_raw_sys::general::{MAP_SHARED, PROT_READ, PROT_WRITE};
use posix_mm::{sys_mmap, sys_munmap};
use posix_types::{UserConstPtr, UserPtr};
use unittest::def_test;
use unittest_support::TestUserValue;

use super::*;

fn setup_ring(entries: u32) -> (i32, IoUringParams, usize, usize, usize) {
    let params = TestUserValue::<IoUringParams>::from_value(IoUringParams::default())
        .expect("params user value");
    let fd = sys_io_uring_setup(entries, UserPtr::from(params.as_user_ptr() as usize))
        .expect("io_uring_setup must succeed") as i32;
    let value = params.read();
    assert_eq!(value.sq_entries, entries);
    assert_eq!(value.cq_entries, entries);
    assert_ne!(value.features & IORING_FEAT_SINGLE_MMAP, 0);

    let sq_len = value.sq_off.array as usize + value.sq_entries as usize * 4;
    let cq_len = value.cq_off.cqes as usize + value.cq_entries as usize * 16;
    let ring_len = memaddr::align_up_4k(sq_len.max(cq_len));
    let ring_va = sys_mmap(
        0,
        ring_len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        IORING_OFF_SQ_RING as i64,
    )
    .expect("SQ/CQ ring mmap") as usize;
    let sqe_va = sys_mmap(
        0,
        entries as usize * 64,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        IORING_OFF_SQES as i64,
    )
    .expect("SQE mmap") as usize;
    (fd, value, ring_va, sqe_va, ring_len)
}

#[def_test(user, serial)]
fn setup_enter_publishes_the_submitted_completion() {
    let (fd, params, ring_va, sqe_va, ring_len) = setup_ring(8);

    UserPtr::<u32>::from(ring_va + params.sq_off.array as usize)
        .write_vm(0)
        .expect("write SQ array index");
    UserPtr::from(sqe_va)
        .write_vm(IoUringSqe {
            opcode: IORING_OP_NOP,
            user_data: 0x5A5A_1234,
            ..IoUringSqe::default()
        })
        .expect("write SQE");
    UserPtr::<u32>::from(ring_va + params.sq_off.tail as usize)
        .write_vm(1)
        .expect("publish SQ tail");

    let submitted = sys_io_uring_enter(
        fd,
        1,
        1,
        IORING_ENTER_GETEVENTS,
        UserConstPtr::default(),
        0,
    )
    .expect("io_uring_enter must succeed");
    assert_eq!(submitted, 1);

    let cqe: IoUringCqe = UserPtr::from(ring_va + params.cq_off.cqes as usize)
        .read_vm()
        .expect("read CQE");
    assert_eq!(cqe.user_data, 0x5A5A_1234);
    assert_eq!(cqe.res, 0);

    UserPtr::<u32>::from(ring_va + params.cq_off.head as usize)
        .write_vm(1)
        .expect("consume CQE");
    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}

#[def_test(user, serial)]
fn setup_and_enter_reject_invalid_input() {
    let mut params = TestUserValue::<IoUringParams>::from_value(IoUringParams::default())
        .expect("params user value");
    let ptr = UserPtr::from(params.as_user_ptr() as usize);

    assert_eq!(
        sys_io_uring_setup(0, ptr),
        Err(KError::InvalidInput),
        "zero entry counts are rejected"
    );
    assert_eq!(
        sys_io_uring_setup(u32::MAX, ptr),
        Err(KError::InvalidInput),
        "oversized entry counts are rejected"
    );

    let mut bad_flags = IoUringParams {
        flags: 1,
        ..IoUringParams::default()
    };
    params.write(bad_flags);
    assert_eq!(
        sys_io_uring_setup(8, ptr),
        Err(KError::InvalidInput),
        "unknown setup flags are rejected"
    );
    bad_flags.flags = 0;
    params.write(bad_flags);

    let (fd, _params, ring_va, sqe_va, ring_len) = setup_ring(8);
    assert_eq!(
        sys_io_uring_enter(fd, 0, 0, 1 << 7, UserConstPtr::default(), 0),
        Err(KError::InvalidInput),
        "unknown enter flags are rejected"
    );
    sys_munmap(sqe_va, 8 * 64).expect("munmap SQEs");
    sys_munmap(ring_va, ring_len).expect("munmap ring");
    posix_fs::sys_close(fd).expect("close");
}
