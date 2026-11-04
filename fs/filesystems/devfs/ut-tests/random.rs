// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::{
    DeviceFileOps, Filename, Mount, NodeFlags, NodePermission, NodeType, Path, SuperBlockFlags,
};
use unittest::{assert, assert_eq, assert_ne, def_test};

use super::*;
use crate::nodes::test_device::open_character_node;

fn random_node() -> EntropyNode {
    EntropyNode::new(Kind::Random)
}

fn urandom_node() -> EntropyNode {
    EntropyNode::new(Kind::Urandom)
}

/// Covers shared devtmpfs publication and VFS reads of the pool-backed nodes,
/// not the identity, availability or output quality of a hardware RNG source.
#[def_test]
fn shared_devtmpfs_random_nodes_are_readable_character_devices() {
    let super_block = crate::new_devfs(SuperBlockFlags::empty()).unwrap();
    let second_mount = crate::new_devfs(SuperBlockFlags::empty()).unwrap();
    assert!(alloc::sync::Arc::ptr_eq(&super_block, &second_mount));
    let root = Path::new(Mount::new_root(&super_block), super_block.root_dir());
    for (name, minor) in [("random", 8), ("urandom", 9)] {
        let file = Filename::new(name)
            .open_with_flags_at(
                &root,
                &root,
                0,
                NodePermission::default(),
                NodePermission::default(),
                kcred::initial_cred(),
            )
            .expect("shared devtmpfs must publish readable random nodes");
        let node_type = file.node_type();
        let device_id = file.inode().rdev();
        let mut buffer = [0xa5u8; 40];
        let result = file.read(&mut buffer[4..36]);
        drop(file);
        assert_eq!(node_type, NodeType::CharacterDevice);
        assert_eq!(device_id, DeviceId::new(1, minor));
        assert_eq!(result, Ok(32));
        assert_eq!(&buffer[..4], &[0xa5; 4]);
        assert_eq!(&buffer[36..], &[0xa5; 4]);
    }
}

#[def_test]
fn test_random_read_fills_buffer() {
    let dev = random_node();
    let mut buf = [0u8; 64];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 64);
    assert!(buf.iter().any(|&b| b != 0));
}

#[def_test]
fn test_random_read_empty() {
    let dev = random_node();
    let mut buf = [];
    let n = dev.read_bytes(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_random_write_accepts_all() {
    let dev = random_node();
    let data = [0u8; 32];
    let n = dev.write_bytes(&data).unwrap();
    assert_eq!(n, 32);
}

#[def_test]
fn test_random_write_empty() {
    let dev = random_node();
    let n = dev.write_bytes(&[]).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_random_supports_rw() {
    let dev = random_node();
    assert!(dev.supports_read());
    assert!(dev.supports_write());
    assert!(dev.flags().contains(NodeFlags::NON_CACHEABLE));
}

#[def_test]
fn test_random_two_reads_differ() {
    let dev = random_node();
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    dev.read_bytes(&mut buf1).unwrap();
    dev.read_bytes(&mut buf2).unwrap();
    assert_ne!(buf1, buf2);
}

#[def_test]
fn test_random_write_changes_stream() {
    let dev = random_node();
    let mut before = [0u8; 32];
    let mut after = [0u8; 32];
    entropy::init();
    dev.read_bytes(&mut before).unwrap();
    dev.write_bytes(b"entropy").unwrap();
    dev.read_bytes(&mut after).unwrap();
    assert_ne!(before, after);
}

/// `/dev/random` is `random_fops`, which provides `random_poll`: `epoll_ctl`
/// accepts the node, and the reported events follow the seeded/unseeded split.
#[def_test(serial)]
fn dev_random_provides_a_poll_handler() {
    let file = open_character_node("random", Arc::new(random_node()));
    assert!(file.can_poll());
    // `random_poll`: readable once seeded, writable before that.
    let expected = if entropy::is_ready() {
        IoEvents::IN | IoEvents::RDNORM
    } else {
        IoEvents::OUT | IoEvents::WRNORM
    };
    // `IoEvents` carries no `PartialEq`, so compare the raw bits.
    assert_eq!(
        file.poll_handler().map(|events| events.bits()),
        Some(expected.bits())
    );
}

/// `/dev/urandom` is `urandom_fops`, which provides no poll operation: the
/// `epoll_ctl` gate refuses it while `poll`/`select` fall back to always-ready.
#[def_test(serial)]
fn dev_urandom_has_no_poll_handler() {
    let file = open_character_node("urandom", Arc::new(urandom_node()));
    assert!(!file.can_poll());
    assert!(!file.poll().is_empty());
}
