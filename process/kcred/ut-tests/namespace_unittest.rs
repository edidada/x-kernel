// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::LinuxError;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_namespace_id_monotonic() {
    let id1 = NamespaceId::new();
    let id2 = NamespaceId::new();
    let id3 = NamespaceId::new();
    assert!(id2.as_u64() > id1.as_u64());
    assert!(id3.as_u64() > id2.as_u64());
}

#[def_test]
fn test_namespace_id_display() {
    let id = NamespaceId::new();
    let displayed = alloc::format!("{}", id);
    assert!(!displayed.is_empty());
}

#[def_test]
fn test_initial_user_namespace_is_singleton() {
    let first = initial_user_namespace();
    let second = initial_user_namespace();
    assert!(Arc::ptr_eq(&first, &second));
}

#[def_test]
fn id_map_extent_rejects_empty_and_wrapping_ranges() {
    assert_eq!(
        LinuxError::from(IdMapExtent::new(0, 0, 0).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(2, 0, u32::MAX).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(0, 2, u32::MAX).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(u32::MAX, 0, 1).unwrap_err()),
        LinuxError::EINVAL
    );
}

#[def_test]
fn unprivileged_child_can_write_its_own_single_extent_maps() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());

    assert!(!namespace.is_ready());
    assert_eq!(
        LinuxError::from(namespace.write_uid_map(&writer, alloc::vec![]).unwrap_err()),
        LinuxError::EINVAL
    );
    assert!(!namespace.has_uid_map());

    namespace
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    assert!(namespace.has_uid_map());
    assert!(!namespace.is_ready());
    assert_eq!(namespace.map_uid_to_initial(0), Some(1000));
    assert_eq!(namespace.map_uid_from_initial(1000), Some(0));

    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    namespace.set_setgroups_allowed(false).unwrap();
    namespace
        .write_gid_map(&writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    assert!(namespace.is_ready());
    assert_eq!(namespace.map_gid_to_initial(0), Some(1000));
}

#[def_test]
fn id_map_is_write_once_per_kind() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());
    let extent = IdMapExtent::new(0, 1000, 1).unwrap();

    namespace
        .write_uid_map(&writer, alloc::vec![extent])
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );

    namespace.set_setgroups_allowed(false).unwrap();
    namespace
        .write_gid_map(&writer, alloc::vec![extent])
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
}

#[def_test]
fn id_map_validation_rejects_overlapping_extents() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());

    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(
                    &writer,
                    alloc::vec![
                        IdMapExtent::new(0, 1000, 2).unwrap(),
                        IdMapExtent::new(1, 1002, 1).unwrap(),
                    ],
                )
                .unwrap_err()
        ),
        LinuxError::EINVAL
    );
    assert!(!namespace.has_uid_map());
}

#[def_test]
fn setgroups_state_machine_matches_linux() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, false).unwrap();

    assert!(namespace.setgroups_allowed());
    namespace.set_setgroups_allowed(false).unwrap();
    assert!(!namespace.setgroups_allowed());
    namespace.set_setgroups_allowed(false).unwrap();
    assert_eq!(
        LinuxError::from(namespace.set_setgroups_allowed(true).unwrap_err()),
        LinuxError::EPERM
    );

    let mapped = UserNamespace::new_child(&parent, 1000, false).unwrap();
    let writer = Cred::root();
    mapped
        .write_gid_map(&writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
        .unwrap();
    assert_eq!(
        LinuxError::from(mapped.set_setgroups_allowed(false).unwrap_err()),
        LinuxError::EPERM
    );
}

#[def_test]
fn mapping_host_root_requires_setfcap() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, false).unwrap();
    let owner = Cred::new(1000, 1000).with_user_namespace(namespace.clone());
    let root_extent = IdMapExtent::new(0, 0, 1).unwrap();

    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&owner, alloc::vec![root_extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    assert!(!namespace.has_uid_map());

    namespace
        .write_uid_map(&Cred::root(), alloc::vec![root_extent])
        .unwrap();
}

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test fixture: the parent map is built from \
              constants that the user-namespace API accepts"
)]
fn parent_namespace_with_map() -> Arc<UserNamespace> {
    let parent = UserNamespace::new_child(&initial_user_namespace(), 1000, false).unwrap();
    let writer = Cred::root();
    parent
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    parent
        .write_gid_map(&writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    parent
}

#[def_test]
fn nested_map_must_be_covered_by_the_parent_map() {
    let parent = parent_namespace_with_map();
    let namespace = UserNamespace::new_child(&parent, 0, false).unwrap();
    let writer = Cred::root();

    assert!(!parent.maps_range_to_parent(MapKind::Uid, 1, 1));
    assert!(parent.maps_range_to_parent(MapKind::Uid, 0, 1));
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, alloc::vec![IdMapExtent::new(0, 1, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    namespace
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
        .unwrap();
    assert_eq!(namespace.map_uid_to_initial(0), Some(1000));
    assert_eq!(namespace.map_uid_from_initial(1000), Some(0));
}

#[def_test]
fn nested_maps_are_translated_through_every_ancestor() {
    let writer = Cred::root();
    let first = UserNamespace::new_child(&initial_user_namespace(), 5, false).unwrap();
    first
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(5, 100, 1).unwrap()])
        .unwrap();

    let second = UserNamespace::new_child(&first, 7, false).unwrap();
    second
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(7, 5, 1).unwrap()])
        .unwrap();

    let third = UserNamespace::new_child(&second, 9, false).unwrap();
    assert!(second.maps_range_to_parent(MapKind::Uid, 7, 1));
    assert!(!second.maps_range_to_parent(MapKind::Uid, 8, 1));
    assert_eq!(
        LinuxError::from(
            third
                .write_uid_map(&writer, alloc::vec![IdMapExtent::new(9, 8, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );

    third
        .write_uid_map(&writer, alloc::vec![IdMapExtent::new(9, 7, 1).unwrap()])
        .unwrap();
    assert!(third.maps_range_to_parent(MapKind::Uid, 9, 1));
    assert!(!third.maps_range_to_parent(MapKind::Uid, 8, 1));
    assert!(!third.maps_range_to_parent(MapKind::Uid, 9, 2));
    assert_eq!(third.map_uid_to_initial(9), Some(100));
    assert_eq!(third.map_uid_from_initial(100), Some(9));
}

#[def_test]
fn namespace_owner_administers_its_direct_child() {
    let initial = initial_user_namespace();
    // The owner of a child namespace has all capabilities over it when acting
    // from the direct parent (Linux `cap_capable` rule 1).
    let child = UserNamespace::new_child(&initial, 1000, false).unwrap();
    let owner = Cred::new(1000, 1000);
    assert!(owner.has_cap_in(&child, CAP_SYS_ADMIN));
    assert!(owner.has_cap_in(&child, CAP_SETUID));
    // A non-owner with the same numeric ID in the parent has no such grant.
    let other = Cred::new(1001, 1001);
    assert!(!other.has_cap_in(&child, CAP_SYS_ADMIN));
    // Root of the parent still reaches the child through its effective set.
    assert!(Cred::root().has_cap_in(&child, CAP_SYS_ADMIN));
}

#[def_test]
fn capabilities_never_flow_toward_ancestors() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, false).unwrap();
    let grandchild = UserNamespace::new_child(&child, 0, false).unwrap();

    // A credential in the child namespace cannot administer the parent, even
    // though the child namespace is fully privileged inside itself.
    let child_root = Cred::root_in_user_namespace(child.clone());
    assert!(!child_root.has_cap_in(&initial, CAP_SYS_ADMIN));

    // Nor can it administer an unrelated sibling.
    let sibling = UserNamespace::new_child(&initial, 2000, false).unwrap();
    assert!(!child_root.has_cap_in(&sibling, CAP_SYS_ADMIN));

    // It does hold authority over its own descendants.
    assert!(child_root.has_cap_in(&grandchild, CAP_SYS_ADMIN));
}

#[def_test]
fn grandchild_owner_grant_requires_the_direct_parent() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, false).unwrap();
    // UID 0 in the child creates the grandchild.
    let grandchild = UserNamespace::new_child(&child, 0, false).unwrap();

    // The grandchild's owner grant is evaluated from its direct parent (the
    // child namespace), where the owner's credential lives.
    let child_root = Cred::root_in_user_namespace(child.clone());
    assert!(child_root.has_cap_in(&grandchild, CAP_SYS_ADMIN));

    // The grandchild's indirect owner (UID 1000 created the child in the
    // initial namespace) still reaches it: Linux applies the owner rule at
    // the child level while walking up, which covers all descendants.
    assert!(Cred::new(1000, 1000).has_cap_in(&grandchild, CAP_SYS_ADMIN));
    // An unrelated ID in the initial namespace has no grant.
    let unprivileged = Cred::new(2000, 2000);
    assert!(!unprivileged.has_cap_in(&grandchild, CAP_SYS_ADMIN));
}

#[def_test]
fn namespace_nesting_is_capped_at_linux_depth() {
    let mut namespace = initial_user_namespace();
    for _ in 0..33 {
        namespace = UserNamespace::new_child(&namespace, 0, false).unwrap();
    }
    // Level 34 is beyond Linux's `parent->level > 32` limit.
    assert_eq!(
        LinuxError::from(
            UserNamespace::new_child(&namespace, 0, false)
                .unwrap_err()
        ),
        LinuxError::ENOSPC
    );
}
