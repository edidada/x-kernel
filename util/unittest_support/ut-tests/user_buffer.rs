// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;
use kprocess::AsThread;

use super::TestUserBuffer;

#[def_test(user)]
fn user_buffer_rounding_zeroing_and_scoped_unmap() {
    let empty = TestUserBuffer::new(0).unwrap();
    assert_eq!(empty.mapped_size, PAGE);
    assert_eq!(empty.len, 0);
    drop(empty);
    let buffer = TestUserBuffer::new(2 * PAGE + 1).unwrap();
    assert_eq!(buffer.read_bytes(buffer.len).unwrap(), alloc::vec![0u8; buffer.len]);
    let address = memaddr::VirtAddr::from_usize(buffer.user_addr);
    let task = ktask::current();
    let aspace = task.try_as_thread().unwrap().process().address_space().unwrap();
    assert_eq!(buffer.mapped_size, 3 * PAGE);
    buffer.write_bytes(&alloc::vec![0x5au8; 2 * PAGE + 1]).unwrap();
    assert_eq!(buffer.read_bytes(buffer.len).unwrap(), alloc::vec![0x5au8; buffer.len]);
    drop(buffer);
    assert!(aspace.lock().find_vma(address).is_none());
    assert!(TestUserBuffer::new(usize::MAX).is_err());
}
