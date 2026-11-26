// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};
use core::ops::Range;

use iov_iter::{IovIterSource, IovSource, iov_iter_kvec_source, iov_iter_source};
use ktime_types::{NANOS_PER_SEC, SystemTime};
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    BufferedOverwriteOperations, GetattrQueryFlags, GetattrRequestMask, NodePermission, OpenFlags,
    StatFs, SuperBlockFlags, SuperBlockOperations, TimestampLimits,
};

struct TestChrdevInode {
    metadata: Mutex<Metadata>,
    last_update: Mutex<Option<MetadataUpdate>>,
    next_applied_update: Mutex<Option<VfsResult<MetadataUpdate>>>,
}

impl TestChrdevInode {
    fn new(rdev: DeviceId) -> Self {
        Self {
            metadata: Mutex::new(Metadata {
                device: 0,
                inode: 42,
                nlink: 1,
                mode: Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 0,
                blocks: 0,
                rdev,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
            }),
            last_update: Mutex::new(None),
            next_applied_update: Mutex::new(None),
        }
    }
}

impl InodeOperations for TestChrdevInode {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&crate::Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(self.metadata.lock().clone())
    }

    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        *self.last_update.lock() = Some(update.clone());
        self.next_applied_update.lock().take().unwrap_or(Ok(update))
    }
}

impl FileOperations for TestChrdevInode {}

impl AddressSpaceOperations for TestChrdevInode {}

impl InodeAttributeOperations for TestChrdevInode {
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn fill_metadata(&self, inode_number: u64, block_size: u64) -> Metadata {
        let mut metadata = self.metadata.lock().clone();
        metadata.inode = inode_number;
        metadata.block_size = block_size;
        metadata
    }

    fn times(&self) -> InodeTimes {
        let metadata = self.metadata.lock();
        InodeTimes {
            accessed_at: metadata.atime,
            modified_at: metadata.mtime,
            changed_at: metadata.ctime,
        }
    }

    fn mode(&self) -> Umode {
        self.metadata.lock().mode
    }

    fn owner(&self) -> (u32, u32) {
        let metadata = self.metadata.lock();
        (metadata.uid, metadata.gid)
    }

    fn link_count(&self) -> u64 {
        self.metadata.lock().nlink
    }

    fn generation(&self) -> u32 {
        17
    }

    fn rdev(&self) -> DeviceId {
        self.metadata.lock().rdev
    }

    fn size(&self) -> u64 {
        self.metadata.lock().size
    }

    fn blocks(&self) -> u64 {
        self.metadata.lock().blocks
    }

    fn set_permission(&self, permission: NodePermission) {
        let mut metadata = self.metadata.lock();
        metadata.mode = Umode::new(metadata.mode.node_type(), permission);
    }

    fn set_owner(&self, uid: u32, gid: u32) {
        let mut metadata = self.metadata.lock();
        metadata.uid = uid;
        metadata.gid = gid;
    }

    fn set_link_count(&self, link_count: u64) {
        self.metadata.lock().nlink = link_count;
    }

    fn increment_link_count(&self) {
        let mut metadata = self.metadata.lock();
        metadata.nlink = metadata.nlink.saturating_add(1);
    }

    fn decrement_link_count(&self) {
        let mut metadata = self.metadata.lock();
        metadata.nlink = metadata.nlink.saturating_sub(1);
    }

    fn set_size(&self, size: u64) {
        self.metadata.lock().size = size;
    }

    fn set_accessed_at(&self, value: SystemTime) {
        self.metadata.lock().atime = value;
    }

    fn set_modified_at(&self, value: SystemTime) {
        self.metadata.lock().mtime = value;
    }

    fn set_changed_at(&self, value: SystemTime) {
        self.metadata.lock().ctime = value;
    }

    fn set_allocated_bytes(&self, bytes: u64) {
        self.metadata.lock().blocks = bytes / INODE_BYTES_PER_BLOCK;
    }

    fn add_allocated_bytes(&self, bytes: u64) {
        let mut metadata = self.metadata.lock();
        metadata.blocks = metadata
            .blocks
            .saturating_add(bytes / INODE_BYTES_PER_BLOCK);
    }

    fn subtract_allocated_bytes(&self, bytes: u64) {
        let mut metadata = self.metadata.lock();
        metadata.blocks = metadata
            .blocks
            .saturating_sub(bytes / INODE_BYTES_PER_BLOCK);
    }
}

struct SecondResolutionSuperBlockOperations;

static SECOND_RESOLUTION_SUPER_BLOCK_OPERATIONS: SecondResolutionSuperBlockOperations =
    SecondResolutionSuperBlockOperations;
static SECOND_RESOLUTION_FILE_SYSTEM_TYPE: crate::FileSystemType =
    crate::FileSystemType::internal("timestamp-limits-test");

impl SuperBlockOperations for SecondResolutionSuperBlockOperations {
    fn statfs(&self, _super_block: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0,
            block_size: 512,
            blocks: 1,
            blocks_free: 1,
            blocks_available: 1,
            file_count: 1,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 512,
        })
    }

    fn timestamp_limits(&self, _super_block: &SuperBlock) -> TimestampLimits {
        TimestampLimits::new(NANOS_PER_SEC as u32, i32::MIN as i64, i32::MAX as i64)
    }
}

fn second_resolution_super_block() -> Arc<SuperBlock> {
    SuperBlock::new_with_flags_and_private(
        &SECOND_RESOLUTION_FILE_SYSTEM_TYPE,
        &SECOND_RESOLUTION_SUPER_BLOCK_OPERATIONS,
        kalloc_types::KBox::new(()).unwrap(),
        SuperBlockFlags::empty(),
        1,
        crate::MAX_LFS_FILESIZE,
        |_| {
            Dentry::new_dir_from_inode(
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        1,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                ),
                None,
                String::new(),
            )
        },
    )
}

#[def_test]
fn special_inodes_preserve_rdev() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
    assert_eq!(inode.metadata().inode, 7);
    assert_eq!(inode.metadata().rdev, DeviceId::new(1, 3));
    assert_eq!(inode.grow_size_after_backing_write(16), Ok(()));
    assert_eq!(inode.size(), 0);
}

struct TimestampPolicyOperations;

impl InodeOperations for TimestampPolicyOperations {
    fn setattr(
        &self,
        _idmap: &MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        let inode = dentry.vfs_inode();
        core::assert!(inode.i_rwsem.try_write().is_none());
        core::assert!(inode.data_lock.try_write().is_none());
        Ok(update)
    }
}

impl FileOperations for TimestampPolicyOperations {}

fn timestamp_policy_path(flags: NodeFlags) -> Path {
    let super_block = crate::nullfs::new_superblock();
    let mount = crate::Mount::new_root(&super_block);
    let operations = Arc::new(TimestampPolicyOperations);
    let inode = VfsInode::new_file_with_operations(
        Arc::new(()),
        operations.clone(),
        operations,
        flags,
        VfsInodeInit::new(
            43,
            0,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(1000, 100, 1, DeviceId::default()),
    );
    let dentry = Dentry::new_file_from_inode(inode, None, String::from("timestamp-policy"));
    dentry.bind_super_block(&super_block);
    Path::new(mount, dentry)
}

#[def_test]
fn ordinary_writers_share_privilege_protection_until_metadata_needs_revocation() {
    let regular_inode = |permissions| {
        VfsInode::new_file(
            Arc::new(TimestampPolicyOperations),
            VfsInodeInit::new(43, 0, Umode::new(NodeType::RegularFile, permissions)),
        )
    };
    let inode = regular_inode(NodePermission::OWNER_WRITE);
    {
        let _first = inode.lock_unprivileged_write().unwrap();
        let _second = inode.lock_unprivileged_write().unwrap();
        assert!(inode.try_lock_inode_exclusive().is_none());
    }
    {
        let _exclusive = inode.try_lock_inode_exclusive().unwrap();
        inode.mark_extended_attributes();
    }
    assert!(inode.lock_unprivileged_write().is_none());
    for bit in [NodePermission::SET_UID, NodePermission::SET_GID] {
        assert!(regular_inode(bit).lock_unprivileged_write().is_none());
    }
}

struct AdmittedOverwriteOperations {
    admitted_range: Mutex<Option<Range<u64>>>,
}

impl InodeOperations for AdmittedOverwriteOperations {}

impl FileOperations for AdmittedOverwriteOperations {
    fn supports_write(&self) -> bool {
        true
    }

    fn buffered_overwrite_operations(&self) -> Option<&dyn BufferedOverwriteOperations> {
        Some(self)
    }
}

impl BufferedOverwriteOperations for AdmittedOverwriteOperations {
    fn can_overwrite(&self, _file: &crate::VfsFile, range: Range<u64>) -> VfsResult<bool> {
        *self.admitted_range.lock() = Some(range);
        Ok(true)
    }
}

struct ShortOverwriteSource<'a> {
    inode: &'a VfsInode,
    bytes: IovIterSource<'a>,
    has_shared_privilege_guard: bool,
    has_shared_data_guard: bool,
}

impl IovSource for ShortOverwriteSource<'_> {
    fn count(&self) -> usize {
        self.bytes.count()
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize> {
        self.has_shared_privilege_guard = self.inode.i_rwsem.try_write().is_none()
            && self.inode.i_rwsem.try_read().is_some();
        self.has_shared_data_guard =
            self.inode.data_lock.try_write().is_none() && self.inode.data_lock.try_read().is_some();
        // A short copy distinguishes the accepted end from the requested end.
        let len = dst.len().min(2);
        self.bytes.copy_from_iter(&mut dst[..len])
    }
}

#[def_test]
fn admitted_shared_overwrite_holds_guards_and_publishes_the_written_prefix() {
    let operations = Arc::new(AdmittedOverwriteOperations {
        admitted_range: Mutex::new(None),
    });
    let inode = VfsInode::new_file_with_flags(
        operations.clone(),
        NodeFlags::ALWAYS_CACHE,
        VfsInodeInit::new(
            43,
            8,
            Umode::new(NodeType::RegularFile, NodePermission::OWNER_WRITE),
        ),
    );
    let super_block = crate::nullfs::new_superblock();
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, "shared-overwrite".into());
    dentry.bind_super_block(&super_block);
    let path = Path::new(crate::Mount::new_root(&super_block), dentry);
    let open_writer = || {
        crate::dentry_open(
            path.clone(),
            OpenFlags::WRITE_ONLY.bits(),
            kcred::initial_cred(),
        )
        .unwrap()
    };
    let writer = open_writer();
    let other_writer = open_writer();
    writer.set_position(3);
    let generation = inode.content_generation();
    let mut source = ShortOverwriteSource {
        inode: &inode,
        bytes: iov_iter_kvec_source(b"abc"),
        has_shared_privilege_guard: false,
        has_shared_data_guard: false,
    };

    assert_eq!(writer.write_iter(&mut iov_iter_source(&mut source)), Ok(2));
    assert_eq!(*operations.admitted_range.lock(), Some(3..6));
    assert!(source.has_shared_privilege_guard);
    assert!(source.has_shared_data_guard);
    assert_eq!(source.count(), 1);
    assert_eq!(writer.position(), 5);
    assert_eq!(other_writer.position(), 0);
    assert_eq!(inode.content_generation(), generation + 1);
    assert_eq!(inode.size(), 8);
    let mut contents = [0; 8];
    writer
        .mapping()
        .with_folio(0, |folio| {
            if let Some(folio) = folio {
                contents.copy_from_slice(&folio.data()[..8]);
            }
        })
        .unwrap();
    assert_eq!(&contents, b"\0\0\0ab\0\0\0");
}

#[def_test]
fn append_only_timestamp_updates_preserve_touch_authorization() {
    let path = timestamp_policy_path(NodeFlags::APPEND_ONLY);
    let owner = Cred::new(1000, 100);
    let current = Some(SetattrTime::Current);
    let explicit = Some(SetattrTime::Explicit(SystemTime::from_unix_seconds(1)));
    assert_eq!(path.set_times(current, current, &owner), Ok(()));
    assert_eq!(path.metadata().atime, path.metadata().mtime);
    assert_eq!(path.metadata().mtime, path.metadata().ctime);
    for (atime, mtime) in [(explicit, explicit), (current, None), (None, current)] {
        assert_eq!(
            path.set_times(atime, mtime, &owner),
            Err(VfsError::OperationNotPermitted)
        );
    }
    let other = Cred::new(2000, 200);
    assert_eq!(path.set_times(current, current, &other), Ok(()));
    let immutable = timestamp_policy_path(NodeFlags::IMMUTABLE);
    assert_eq!(
        immutable.set_times(current, current, &owner),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(immutable.set_times(None, None, &owner), Ok(()));
}

#[def_test]
fn timestamp_updates_wait_for_namespace_without_holding_data() {
    let path = timestamp_policy_path(NodeFlags::empty());
    let inode = path.inode();
    let namespace = inode.lock_inode_exclusive();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    let task = ktask::spawn(move || {
        *completed.lock() = Some(path.set_times(
            Some(SetattrTime::Current),
            Some(SetattrTime::Current),
            &Cred::new(1000, 100),
        ));
    });
    while !task.state().is_blocked() && task.state() != ktask::TaskState::Exited {
        ktask::yield_now();
    }
    let is_data_available = inode.data_lock.try_write().is_some();
    drop(namespace);
    assert_eq!(task.join(), 0);
    assert_eq!(*result.lock(), Some(Ok(())));
    assert!(
        is_data_available,
        "namespace wait must not retain the data lock"
    );
}

#[def_test]
fn timestamp_authorization_requires_mapped_inode_ids_for_namespace_capability() {
    let path = timestamp_policy_path(NodeFlags::empty());
    let namespace =
        kcred::UserNamespace::new_child(&kcred::initial_user_namespace(), 2000, 200, false)
            .unwrap();
    let caller = Cred::new(2000, 200).with_user_namespace(namespace.clone());
    assert!(caller.has_cap_in(caller.user_ns(), kcred::CAP_FOWNER));
    let explicit = Some(SetattrTime::Explicit(SystemTime::from_unix_seconds(1)));
    assert_eq!(
        path.set_times(explicit, explicit, &caller),
        Err(VfsError::OperationNotPermitted)
    );
    let writer = Cred::root();
    namespace
        .write_uid_map(
            &writer,
            &writer,
            alloc::vec![kcred::IdMapExtent::new(0, 1000, 1).unwrap()],
        )
        .unwrap();
    assert_eq!(
        path.set_times(explicit, explicit, &caller),
        Err(VfsError::OperationNotPermitted)
    );
    namespace
        .write_gid_map(
            &writer,
            &writer,
            alloc::vec![kcred::IdMapExtent::new(0, 100, 1).unwrap()],
        )
        .unwrap();
    assert_eq!(path.set_times(explicit, explicit, &caller), Ok(()));
}

#[def_test]
fn filesystem_attribute_operations_are_the_vfs_inode_state() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let init = VfsInodeInit::new(
        42,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_with_inode_attribute_operations(
        node.clone(),
        empty_address_space_operations(),
        init,
    );

    inode.set_link_count(5);
    inode.set_size(23);
    assert_eq!(node.metadata.lock().nlink, 5);
    assert_eq!(node.metadata.lock().size, 23);

    node.metadata.lock().uid = 1000;
    node.metadata.lock().size = 41;
    assert_eq!(inode.metadata().uid, 1000);
    assert_eq!(inode.size(), 41);
    assert_eq!(inode.generation(), 17);
}

#[def_test]
fn inode_time_updates_use_superblock_timestamp_limits() {
    let super_block = second_resolution_super_block();
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let inode = super_block.get_or_init_inode(42, || {
        VfsInode::new_with_inode_attribute_operations(
            node.clone(),
            empty_address_space_operations(),
            VfsInodeInit::new(
                42,
                0,
                Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
            )
            .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3)),
        )
    });
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, String::from("timestamp"));

    dentry
        .update_metadata(MetadataUpdate {
            atime: Some(SystemTime::from_unix_parts(11, 111).unwrap()),
            mtime: Some(SystemTime::from_unix_parts(12, 222).unwrap()),
            ctime: Some(SystemTime::from_unix_parts(13, 333).unwrap()),
            ..Default::default()
        })
        .unwrap();

    let callback_update = node.last_update.lock().clone().unwrap();
    assert_eq!(
        callback_update.atime,
        Some(SystemTime::from_unix_seconds(11))
    );
    assert_eq!(
        callback_update.mtime,
        Some(SystemTime::from_unix_seconds(12))
    );
    assert_eq!(
        callback_update.ctime,
        Some(SystemTime::from_unix_seconds(13))
    );
    assert_eq!(inode.metadata().atime, SystemTime::from_unix_seconds(11));
    assert_eq!(inode.metadata().mtime, SystemTime::from_unix_seconds(12));
    assert_eq!(inode.metadata().ctime, SystemTime::from_unix_seconds(13));

    inode
        .update_time(
            &dentry,
            inode.truncate_timestamp(SystemTime::from_unix_parts(14, 444).unwrap()),
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();
    let callback_update = node.last_update.lock().clone().unwrap();
    assert_eq!(
        callback_update.mtime,
        Some(SystemTime::from_unix_seconds(14))
    );
    assert_eq!(
        callback_update.ctime,
        Some(SystemTime::from_unix_seconds(14))
    );
    assert_eq!(inode.metadata().mtime, SystemTime::from_unix_seconds(14));
    assert_eq!(inode.metadata().ctime, SystemTime::from_unix_seconds(14));

    assert_eq!(
        inode.truncate_timestamp(SystemTime::from_unix_parts(15, 555).unwrap()),
        SystemTime::from_unix_seconds(15)
    );
    assert_eq!(inode.current_time().subsec_nanos(), 0);
    assert_eq!(
        inode.set_accessed_at(SystemTime::from_unix_parts(15, 555).unwrap()),
        SystemTime::from_unix_parts(15, 555).unwrap()
    );
    assert_eq!(
        inode.metadata().atime,
        SystemTime::from_unix_parts(15, 555).unwrap()
    );
}

#[def_test]
fn resident_timestamps_follow_values_returned_by_filesystem() {
    let super_block = second_resolution_super_block();
    let node = Arc::new(TestChrdevInode::new(DeviceId::new(1, 3)));
    let inode = super_block.get_or_init_inode(43, || {
        VfsInode::new_with_inode_attribute_operations(
            node.clone(),
            empty_address_space_operations(),
            VfsInodeInit::new(
                43,
                0,
                Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
            ),
        )
    });
    let dentry = Dentry::new_file_from_inode(inode.clone(), None, String::from("applied"));
    let applied_atime = SystemTime::from_unix_seconds(20);
    *node.next_applied_update.lock() = Some(Ok(MetadataUpdate {
        atime: Some(applied_atime),
        ..Default::default()
    }));

    dentry
        .update_metadata(MetadataUpdate {
            atime: Some(SystemTime::from_unix_seconds(21)),
            mtime: Some(SystemTime::from_unix_seconds(22)),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(inode.metadata().atime, applied_atime);
    assert_eq!(inode.metadata().mtime, SystemTime::UNIX_EPOCH);

    let applied_cmtime = SystemTime::from_unix_seconds(24);
    *node.next_applied_update.lock() = Some(Ok(MetadataUpdate {
        mtime: Some(applied_cmtime),
        ctime: Some(applied_cmtime),
        ..Default::default()
    }));
    inode
        .update_time(
            &dentry,
            SystemTime::from_unix_seconds(25),
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();

    assert_eq!(inode.metadata().mtime, applied_cmtime);
    assert_eq!(inode.metadata().ctime, applied_cmtime);

    *node.last_update.lock() = None;
    inode
        .update_time(
            &dentry,
            applied_cmtime,
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();
    assert!(node.last_update.lock().is_none());

    *node.next_applied_update.lock() = Some(Err(VfsError::Io));
    assert_eq!(
        inode.update_time(
            &dentry,
            SystemTime::from_unix_seconds(26),
            InodeUpdateTime::ChangeAndModification,
        ),
        Err(VfsError::Io)
    );
    assert_eq!(inode.metadata().atime, applied_atime);
    assert_eq!(inode.metadata().mtime, applied_cmtime);
    assert_eq!(inode.metadata().ctime, applied_cmtime);
}

#[def_test]
fn inode_link_count_helpers_update_link_count() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);

    assert_eq!(inode.link_count(), 1);
    inode.increment_link_count();
    assert_eq!(inode.link_count(), 2);
    inode.decrement_link_count();
    assert_eq!(inode.link_count(), 1);
    inode.increment_link_count();
    assert_eq!(inode.link_count(), 2);
    inode.decrement_link_count();
    assert_eq!(inode.link_count(), 1);
    inode.set_link_count(7);
    assert_eq!(inode.link_count(), 7);
    inode.clear_link_count();
    assert_eq!(inode.link_count(), 0);
}

#[def_test]
fn backing_metadata_refresh_updates_mutable_cached_attributes() {
    let node = Arc::new(TestChrdevInode::new(DeviceId::default()));
    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(0o666),
        ),
    )
    .with_owner_links_and_rdev(0, 0, 1, DeviceId::new(1, 3));
    let inode = VfsInode::new_special(node, NodeFlags::NON_CACHEABLE, init);
    let mut updated = inode.metadata();
    updated.nlink = 0;
    updated.mode = updated
        .mode
        .with_permission(NodePermission::from_bits_truncate(0o640));
    updated.uid = 1000;
    updated.gid = 1001;
    updated.blocks = 7;
    updated.atime = SystemTime::from_unix_parts(11, 1).unwrap();
    updated.mtime = SystemTime::from_unix_parts(12, 2).unwrap();
    updated.ctime = SystemTime::from_unix_parts(13, 3).unwrap();

    inode
        .update_attributes_after_backing_change(&updated)
        .unwrap();

    let cached = inode.metadata();
    assert_eq!(cached.nlink, 0);
    assert_eq!(cached.mode.permission().bits(), 0o640);
    assert_eq!((cached.uid, cached.gid), (1000, 1001));
    assert_eq!(cached.blocks, 7);
    assert_eq!(cached.atime, SystemTime::from_unix_parts(11, 1).unwrap());
    assert_eq!(cached.mtime, SystemTime::from_unix_parts(12, 2).unwrap());
    assert_eq!(cached.ctime, SystemTime::from_unix_parts(13, 3).unwrap());

    updated.inode = 8;
    assert_eq!(
        inode.update_attributes_after_backing_change(&updated),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn metadata_only_inode_operations_accepts_owner_changes() {
    let ops = metadata_only_inode_operations();
    let inode = VfsInode::new_dir(
        Arc::new(EmptyInodeOperations),
        VfsInodeInit::new(
            1,
            0,
            Umode::new(NodeType::Directory, NodePermission::empty()),
        ),
    );
    let dentry = Dentry::new_dir_from_inode(inode, None, String::new());
    let update = MetadataUpdate {
        owner: Some((1, 2)),
        mode: Some(NodePermission::from_bits_truncate(0o600)),
        ..Default::default()
    };
    let applied = ops
        .setattr(&MountIdmap, &dentry, update.clone())
        .expect("metadata-only ops must accept setattr");
    assert_eq!(applied.owner, Some((1, 2)));
    assert_eq!(
        applied.mode.map(|mode| mode.bits()),
        Some(NodePermission::from_bits_truncate(0o600).bits())
    );
}

#[def_test]
fn file_lock_context_allocation_failure_is_retryable() {
    let inode = VfsInode::new_dir(
        Arc::new(EmptyInodeOperations),
        VfsInodeInit::new(
            1,
            0,
            Umode::new(NodeType::Directory, NodePermission::empty()),
        ),
    );
    let result = inode.init_file_locks(|| Err(VfsError::NoMemory));
    assert_eq!(result.err(), Some(VfsError::NoMemory));
    assert!(inode.existing_file_locks().is_none());
    let context = inode.file_locks().unwrap();
    let reused = inode.init_file_locks(|| Err(VfsError::NoMemory)).unwrap();
    assert!(core::ptr::eq(context, reused));
    assert!(core::ptr::eq(context, inode.existing_file_locks().unwrap()));
}
