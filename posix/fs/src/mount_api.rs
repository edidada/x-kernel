// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! New mount API: `fsopen(2)`, `fsconfig(2)`, `fsmount(2)`, `move_mount(2)`.
//!
//! systemd mounts service credential stores and builds service mount
//! namespaces with these calls. An fs-context fd accumulates mount options;
//! `FSCONFIG_CMD_CREATE` constructs the superblock; `fsmount(2)` wraps it in a
//! detached mount; and `move_mount(2)` attaches that mount at a target path.
//! Only the subset systemd exercises is implemented; unsupported requests
//! report `EINVAL`/`EOPNOTSUPP` so callers can fall back.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::ffi::{c_char, c_int, c_void};

use anon_inodefs::AnonInodeFs;
use kerrno::{KError, KResult};
use ksync::Mutex;
use kvfs::{
    FMode, FileOperations, Filename, FsContext, LookupFlags, LookupIntent, MntNamespace, Mount,
    MountFlags, OpenFlags, Path, SuperBlock, SuperBlockFlags, VfsFile, VfsInode, VfsResult,
    dentry_open, path::PATH_MAX,
};
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_RECURSIVE, FSMOUNT_CLOEXEC, FSOPEN_CLOEXEC, MOUNT_ATTR_NODEV,
    MOUNT_ATTR_NOEXEC, MOUNT_ATTR_NOSUID, MOUNT_ATTR_NOSYMFOLLOW, MOUNT_ATTR_RDONLY,
    MOVE_MOUNT_F_EMPTY_PATH, O_DIRECTORY, O_RDONLY, OPEN_TREE_CLOEXEC, OPEN_TREE_CLONE,
};
use osvm::VirtPtr;
use posix_types::UserConstPtr;

use crate::{mount::require_mount_privilege, path::with_fs_at};

// fsconfig(2) commands; linux_raw_sys does not expose them.
const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSCONFIG_CMD_RECONFIGURE: u32 = 7;

// mount_attr(2) atime group; linux_raw_sys exposes the individual bits but not
// the mask used to clear the whole policy.
const MOUNT_ATTR__ATIME: u64 = 0x0000_0070;
const MOUNT_ATTR_NOATIME: u64 = 0x0000_0010;
const MOUNT_ATTR_STRICTATIME: u64 = 0x0000_0020;
const MOUNT_ATTR_NODIRATIME: u64 = 0x0000_0080;

/// `MOUNT_ATTR_SIZE_VER0`: sizeof the first published `struct mount_attr`.
///
/// Linux rejects a `mount_setattr` call whose `usize` is below this, even
/// though the fields it reads live in the first 16 bytes.
const MOUNT_ATTR_SIZE_VER0: usize = 32;

/// Every `MOUNT_ATTR_*` bit `build_mount_kattr` accepts.
///
/// `MOUNT_SETATTR_VALID_FLAGS` in `fs/namespace.c` is `FSMOUNT_VALID_FLAGS`
/// (`RDONLY|NOSUID|NODEV|NOEXEC|__ATIME|NODIRATIME|NOSYMFOLLOW`) plus
/// `MOUNT_ATTR_IDMAP`, i.e. `0x3000ff`. A bit outside this set is `EINVAL`
/// there, so silently dropping it would report success for a request the kernel
/// did not honour.
const MOUNT_ATTR_VALID_FLAGS: u64 = 0x0030_00ff;

/// Rejects an `attr_set`/`attr_clr` word with a bit the ABI does not define.
fn check_mount_attr_bits(attr: u64) -> KResult<()> {
    if attr & !MOUNT_ATTR_VALID_FLAGS != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Rejects an atime request Linux will not honour.
///
/// `build_mount_kattr` requires `MOUNT_ATTR__ATIME` in `attr_clr` whenever the
/// caller sets any atime bit, and requires the group in `attr_clr` to be either
/// all of it or none. Both forms are `EINVAL` there, so accepting them here
/// would report success for a policy change the ABI cannot express.
fn check_mount_attr_atime(attr_set: u64, attr_clr: u64) -> KResult<()> {
    if attr_clr & MOUNT_ATTR__ATIME != 0 && attr_clr & MOUNT_ATTR__ATIME != MOUNT_ATTR__ATIME {
        return Err(KError::InvalidInput);
    }
    if attr_set & MOUNT_ATTR__ATIME != 0 && attr_clr & MOUNT_ATTR__ATIME == 0 {
        return Err(KError::InvalidInput);
    }
    // The atime field is an enum, not a bitmap: `build_mount_kattr` switches on
    // `attr_set & MOUNT_ATTR__ATIME` and its `default` arm is EINVAL, so only
    // RELATIME (0), NOATIME and STRICTATIME are representable. A combination
    // such as `NOATIME | STRICTATIME` (0x30) must be refused rather than
    // silently applied as relatime, which is what a bitwise read would do.
    if attr_clr & MOUNT_ATTR__ATIME != 0 {
        let value = attr_set & MOUNT_ATTR__ATIME;
        if value != 0 && value != MOUNT_ATTR_NOATIME && value != MOUNT_ATTR_STRICTATIME {
            return Err(KError::InvalidInput);
        }
    }
    Ok(())
}

/// Requires the bytes past the published `mount_attr` to be zero.
///
/// `copy_struct_from_user` copies the interoperable prefix and then checks the
/// extra bytes: a non-zero tail means the caller relies on a field this kernel
/// does not implement, so it must fail (`E2BIG`) instead of having its request
/// silently truncated to the fields that are known.
fn check_mount_attr_tail(base: usize, size: usize) -> KResult<()> {
    let tail_len = size.saturating_sub(MOUNT_ATTR_SIZE_VER0);
    if tail_len == 0 {
        return Ok(());
    }
    let tail = UserConstPtr::<u8>::from(base + MOUNT_ATTR_SIZE_VER0).load_vm_vec(tail_len)?;
    if tail.iter().any(|byte| *byte != 0) {
        return Err(KError::ArgumentListTooLong);
    }
    Ok(())
}

/// Accumulated state of one fs-context fd between `fsopen` and `fsmount`.
struct FsContextState {
    fs_type: String,
    source: Option<String>,
    /// Filesystem-specific options rendered as `key=value` / `key` tokens and
    /// joined into the legacy mount-data string at `FSCONFIG_CMD_CREATE`.
    options: Vec<String>,
    sb_flags: SuperBlockFlags,
    super_block: Option<Arc<SuperBlock>>,
}

struct FsContextFd {
    state: Mutex<FsContextState>,
}

/// A detached mount produced by `fsmount` or `open_tree`, consumed by
/// `move_mount`. `_kept` holds strong references to an `open_tree` clone's
/// subtree so the children survive until `move_mount` registers them.
struct MountFd {
    mount: Mutex<Option<Arc<Mount>>>,
    _kept: Vec<Arc<Mount>>,
    /// Where this mount is attached, when the fd names a live mount rather
    /// than a detached one.
    ///
    /// `fsmount` and `open_tree(OPEN_TREE_CLONE)` hand out detached mounts
    /// (`None`); a non-cloning `open_tree` names a mount that is still in the
    /// namespace, and `move_mount` must take it out of `attached_location`
    /// before grafting it somewhere else, as `do_move_mount` does. Without this
    /// the same mount would end up visible at two paths.
    attached_location: Mutex<Option<Path>>,
}

struct MountApiFops;

impl FileOperations for MountApiFops {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        Ok(())
    }
}

fn context_fd(file: &VfsFile) -> KResult<Arc<FsContextFd>> {
    file.private_data_get::<FsContextFd>()
        .ok_or(KError::BadFileDescriptor)
}

fn mount_fd(file: &VfsFile) -> KResult<Arc<MountFd>> {
    file.private_data_get::<MountFd>()
        .ok_or(KError::BadFileDescriptor)
}

/// Opens a filesystem context for `fs_type` (`fsopen(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's credentials and
/// installs the new fd into the caller's fd table. An IRQ handler, a softirq or
/// a kernel task without a current process cannot call it. It may sleep
/// (anonymous-inode file creation and fd allocation), so callers must hold no
/// spinlock and no IRQ-disable or preempt-disable guard.
pub fn sys_fsopen(fs_type: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let fs_type = fs_type.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_fsopen <= fs_type: {fs_type:?}, flags: {flags:#x}");
    if flags & !FSOPEN_CLOEXEC != 0 {
        return Err(KError::InvalidInput);
    }
    // Creating a filesystem context can lead to a new superblock and mount;
    // Linux gates fsopen(2) on CAP_SYS_ADMIN like mount(2).
    require_mount_privilege(&kprocess::current_cred())?;
    kvfs::get_filesystem_type(&fs_type).ok_or(KError::NoSuchDevice)?;
    let state = FsContextState {
        fs_type,
        source: None,
        options: Vec::new(),
        sb_flags: SuperBlockFlags::empty(),
        super_block: None,
    };
    let file = AnonInodeFs::global().get_file(
        "[fscontext]",
        Arc::new(MountApiFops),
        Arc::new(FsContextFd {
            state: Mutex::new(state),
        }),
        FMode::READ | FMode::WRITE,
        OpenFlags::READ_WRITE,
        kprocess::current_cred(),
    )?;
    kprocess::current_files()
        .add_file(file, flags & FSOPEN_CLOEXEC != 0)
        .map(|fd| fd as isize)
}

/// Configures an fs-context fd (`fsconfig(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table and
/// credentials. `FSCONFIG_CMD_CREATE`/`CMD_RECONFIGURE` build or reconfigure a
/// superblock and may sleep (filesystem `get_tree`/`reconfigure` callbacks), so
/// callers must hold no spinlock and no IRQ-disable or preempt-disable guard.
pub fn sys_fsconfig(
    fd: c_int,
    cmd: u32,
    key: UserConstPtr<c_char>,
    value: UserConstPtr<c_void>,
    _aux: c_int,
) -> KResult<isize> {
    let file = kprocess::current_files().get_file(fd)?;
    let ctx = context_fd(&file)?;
    // fsconfig can create or reconfigure a superblock; enforce the same mount
    // authority as fsopen/mount(2) rather than relying on the fd alone.
    require_mount_privilege(&kprocess::current_cred())?;
    let mut state = ctx.state.lock();
    match cmd {
        FSCONFIG_SET_FLAG => {
            let key = key.load_string_with_max_len(PATH_MAX)?;
            // `ro` is a superblock flag, not a filesystem mount option.
            if key == "ro" {
                state.sb_flags.insert(SuperBlockFlags::RDONLY);
            } else {
                state.options.push(key);
            }
        }
        FSCONFIG_SET_STRING => {
            let key = key.load_string_with_max_len(PATH_MAX)?;
            let value = value.cast::<c_char>().load_string_with_max_len(PATH_MAX)?;
            // Linux treats the `source` key as the device/backing source, not a
            // filesystem option; block-device get_tree_bdev reads it from the
            // fs context. Folding it into the option string would both lose the
            // source and pollute the mount data.
            if key == "source" {
                state.source = Some(value);
            } else {
                state.options.push(alloc::format!("{key}={value}"));
            }
        }
        FSCONFIG_CMD_CREATE => {
            if state.super_block.is_some() {
                return Err(KError::InvalidInput);
            }
            let data = state.options.join(",");
            let fs_type = kvfs::get_filesystem_type(&state.fs_type).ok_or(KError::NoSuchDevice)?;
            let process = kprocess::current_user_process();
            let cred = kprocess::current_cred();
            let (root, pwd) = process.fs_context()?.lock().root_and_pwd();
            let mut context = FsContext::new(
                fs_type,
                state.source.as_deref(),
                Some(data.as_bytes()),
                state.sb_flags,
                &cred,
            )?;
            state.super_block = Some(context.get_tree(&root, &pwd)?);
        }
        FSCONFIG_CMD_RECONFIGURE => {
            let super_block = state.super_block.as_ref().ok_or(KError::InvalidInput)?;
            let cred = kprocess::current_cred();
            let mut context = FsContext::new_reconfigure(
                super_block.as_ref(),
                state.source.as_deref(),
                None,
                state.sb_flags,
                state.sb_flags,
                &cred,
            )?;
            super_block.reconfigure(&mut context)?;
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(0)
}

/// Maps `fsmount(2)` `MOUNT_ATTR_*` bits to per-mount [`MountFlags`].
fn mount_attr_to_flags(attr: u32) -> MountFlags {
    let mut flags = MountFlags::empty();
    if attr & MOUNT_ATTR_RDONLY != 0 {
        flags |= MountFlags::RDONLY;
    }
    if attr & MOUNT_ATTR_NOSUID != 0 {
        flags |= MountFlags::NOSUID;
    }
    if attr & MOUNT_ATTR_NODEV != 0 {
        flags |= MountFlags::NODEV;
    }
    if attr & MOUNT_ATTR_NOEXEC != 0 {
        flags |= MountFlags::NOEXEC;
    }
    if attr & MOUNT_ATTR_NOSYMFOLLOW != 0 {
        flags |= MountFlags::NOSYMFOLLOW;
    }
    flags
}

/// Creates a detached mount from a prepared fs-context fd (`fsmount(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table and mount
/// namespace. It may sleep (`dentry_open` of the detached root and fd
/// allocation), so callers must hold no spinlock and no IRQ-disable or
/// preempt-disable guard.
pub fn sys_fsmount(fd: c_int, flags: u32, attr_flags: u32) -> KResult<isize> {
    debug!("sys_fsmount <= fd: {fd}, flags: {flags:#x}, attr_flags: {attr_flags:#x}");
    if flags & !FSMOUNT_CLOEXEC != 0 {
        return Err(KError::InvalidInput);
    }
    // fsmount creates a detached mount; Linux requires CAP_SYS_ADMIN.
    require_mount_privilege(&kprocess::current_cred())?;
    let file = kprocess::current_files().get_file(fd)?;
    let ctx = context_fd(&file)?;
    let (super_block, source) = {
        let state = ctx.state.lock();
        let super_block = state.super_block.clone().ok_or(KError::InvalidInput)?;
        (super_block, state.source.clone())
    };
    let namespace = kprocess::current_user_process().mnt_ns()?;
    let mount = namespace.create_detached_mount(
        &super_block,
        mount_attr_to_flags(attr_flags),
        source.as_deref(),
        None,
    )?;
    // The fsmount fd must be openable as the new mount's root directory so the
    // caller can populate it (systemd writes credentials) before move_mount
    // attaches it. Open the detached root and stash the mount for move_mount.
    let file = dentry_open(
        mount.root_path(),
        O_RDONLY | O_DIRECTORY,
        kprocess::current_cred(),
    )?;
    file.set_private_data(Arc::new(MountFd {
        mount: Mutex::new(Some(mount)),
        _kept: Vec::new(),
        attached_location: Mutex::new(None),
    }));
    kprocess::current_files()
        .add_file(file, flags & FSMOUNT_CLOEXEC != 0)
        .map(|fd| fd as isize)
}

/// Attaches a detached mount at a target path (`move_mount(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table, mount
/// namespace and credentials. It may sleep (target pathname lookup and mount
/// graft), so it must not be called from IRQ context or while holding a
/// spinlock or IRQ-preemption guard.
pub fn sys_move_mount(
    from_fd: c_int,
    from_path: UserConstPtr<c_char>,
    to_fd: c_int,
    to_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let from_path = from_path.load_string_with_max_len(PATH_MAX)?;
    let to_path = to_path.load_string_with_max_len(PATH_MAX)?;
    debug!(
        "sys_move_mount <= from_fd: {from_fd}, to_fd: {to_fd}, to_path: {to_path:?}, flags: \
         {flags:#x}"
    );
    if flags & !MOVE_MOUNT_F_EMPTY_PATH != 0 {
        return Err(KError::InvalidInput);
    }
    if !from_path.is_empty() {
        // Moving an already-attached mount by path is not implemented; only
        // detached mounts from fsmount(2) (empty from_path) are supported.
        return Err(KError::InvalidInput);
    }
    // move_mount grafts a mount into the namespace; Linux requires CAP_SYS_ADMIN.
    let cred = kprocess::current_cred();
    require_mount_privilege(&cred)?;

    // Resolve the target before consuming the fsmount mount so a failed lookup
    // (ENOENT, invalid to_fd, ...) leaves the descriptor usable for a retry,
    // matching Linux, which looks up to_path before reading the from fd.
    //
    // Linux resolves to_path without following the final component unless the
    // caller passes MOVE_MOUNT_T_SYMLINKS; a symlink target is then refused by
    // the directory check instead of being mounted over. This path rejects
    // every flag except MOVE_MOUNT_F_EMPTY_PATH, so the no-follow lookup is the
    // only behaviour the accepted flag set can produce.
    let filename = Filename::new(to_path.as_str());
    let target = with_fs_at(to_fd, &filename, |fs| {
        filename.lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::no_follow(),
            &cred,
        )
    })?;
    let namespace = kprocess::current_user_process().mnt_ns()?;

    let from_file = kprocess::current_files().get_file(from_fd)?;
    let mfd = mount_fd(&from_file)?;
    // Hold the fsmount slot lock across take-attach-restore so a concurrent
    // move_mount on the same fd cannot observe the transient empty slot or
    // interleave with the rollback (CG-015: one critical section).
    let mut slot = mfd.mount.lock();
    let mount = slot.take().ok_or(KError::InvalidInput)?;
    // A non-cloning `open_tree` fd names a mount that is still attached. Linux's
    // `do_move_mount` detaches the source before grafting it, so the mount is
    // never visible at two paths; skipping this grafted a second copy and left
    // the original in place. Take the recorded location first so a failed
    // detach leaves the fd usable.
    let mut attached_location = mfd.attached_location.lock();
    // A graft can be rejected (unregistered mountpoint, directory/non-directory
    // mismatch). Detaching first would then have destroyed the source for a
    // move that cannot happen, so the same checks run before anything changes.
    if let Err(error) = namespace.check_detached_mount_graft(&mount, &target) {
        *slot = Some(mount);
        return Err(error);
    }
    let previous_location = attached_location.take();
    if let Some(location) = previous_location.as_ref()
        && let Err(error) = namespace.detach_for_move(location)
    {
        *attached_location = previous_location;
        *slot = Some(mount);
        return Err(error);
    }
    if let Err(error) = namespace.attach_detached_mount(mount.clone(), &target) {
        // `attach_detached_mount` fails before installing, so restore both the
        // fd's mount slot and, when the source was detached, its old location.
        // Only the edge to the old parent was removed, so re-grafting the root
        // restores the tree with its subtree intact.
        if let Some(location) = previous_location {
            let _ = namespace.attach_detached_mount(mount.clone(), &location);
            *attached_location = Some(location);
        }
        *slot = Some(mount);
        return Err(error);
    }
    Ok(0)
}

/// Clones the mount subtree at a path into a detached mount fd (`open_tree(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it checks the caller's credentials
/// (`OPEN_TREE_CLONE` needs CAP_SYS_ADMIN), resolves a pathname against its
/// root/pwd/mount namespace and allocates a descriptor in its fd table. An IRQ
/// handler, a softirq or a kernel task without a current process cannot call
/// it. It may sleep while resolving the path (`lookup_at`) and cloning the
/// subtree, so callers must hold no spinlock and no IRQ-disable or
/// preempt-disable guard. The namespace registry lock is taken and released
/// inside the call; a caller must not already hold it (nor the inode locks of
/// the resolved path) when entering.
pub fn sys_open_tree(dfd: c_int, path: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let path = path.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_open_tree <= dfd: {dfd}, path: {path:?}, flags: {flags:#x}");
    // Linux `open_tree`: the accepted word is exactly the OPEN_TREE_* pair plus
    // the four path-resolution hints, and `AT_RECURSIVE` without
    // `OPEN_TREE_CLONE` is rejected because there is nothing to recurse into.
    if flags & !(OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_EMPTY_PATH | AT_RECURSIVE | 0x100 | 0x800)
        != 0
    {
        return Err(KError::InvalidInput);
    }
    if flags & AT_RECURSIVE != 0 && flags & OPEN_TREE_CLONE == 0 {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();
    let is_clone = flags & OPEN_TREE_CLONE != 0;
    // Cloning a mount into a detached tree is a mount operation: Linux gates it
    // on `may_mount()`, which every other mount entry point here also requires.
    if is_clone {
        require_mount_privilege(&cred)?;
    }
    let filename = Filename::new(path.as_str());
    // `open_tree` passes AT_NO_AUTOMOUNT/AT_SYMLINK_NOFOLLOW/AT_EMPTY_PATH
    // straight into `user_path_at`; only the last two change this walk.
    let mut lookup = LookupFlags::follow();
    if flags & 0x100 != 0 {
        // AT_SYMLINK_NOFOLLOW: the final component must not be followed, so
        // opening a symlink names the link's own mount.
        lookup = LookupFlags::no_follow();
    }
    if flags & AT_EMPTY_PATH != 0 {
        lookup |= LookupFlags::EMPTY_PATH;
    }
    let source = with_fs_at(dfd, &filename, |fs| {
        filename.lookup_at(fs.root(), fs.pwd(), LookupIntent::Open, lookup, &cred)
    })?;
    let (root, kept, open_path) = if is_clone {
        let namespace = kprocess::current_user_process().mnt_ns()?;
        // Linux's `__do_loopback` clones only the named mount unless the caller
        // passed `AT_RECURSIVE`.
        let (root, kept) =
            namespace.clone_mount_tree_recursive(&source, flags & AT_RECURSIVE != 0)?;
        let open_path = root.root_path();
        (root, kept, open_path)
    } else {
        // Non-cloning open_tree: a reference to the existing mount at the
        // resolved path, usable with mount_setattr(AT_EMPTY_PATH) and
        // move_mount. The fd must keep the resolved dentry rather than the
        // mount root: callers such as systemd's chase() resolve further
        // components relative to it, and returning the mount root would reset
        // the walk to the mountpoint on every non-mountpoint component.
        (source.mount().clone(), Vec::new(), source.clone())
    };
    // `dentry_open(&path, O_PATH, ...)`: the descriptor names the mount without
    // granting read access to whatever it covers, which is why `open_tree`
    // works on a directory it may not read.
    let file = dentry_open(open_path.clone(), OpenFlags::PATH.bits(), cred)?;
    file.set_private_data(Arc::new(MountFd {
        mount: Mutex::new(Some(root)),
        // A clone is detached; a non-cloning fd names the live mount at the
        // resolved path, which `move_mount` has to detach first.
        attached_location: Mutex::new(if is_clone {
            None
        } else {
            Some(open_path.clone())
        }),
        _kept: kept,
    }));
    kprocess::current_files()
        .add_file(file, flags & OPEN_TREE_CLOEXEC != 0)
        .map(|fd| fd as isize)
}

/// Translates `mount_setattr(2)` `attr_set`/`attr_clr` into per-mount flags.
///
/// The atime field is an enum, not a bitmap, so a caller changes it by putting
/// `MOUNT_ATTR__ATIME` in `attr_clr` and the wanted value in `attr_set`
/// (`build_mount_kattr`). `MOUNT_ATTR_RELATIME` is zero, so "request relatime"
/// is exactly `attr_clr = MOUNT_ATTR__ATIME, attr_set = 0` -- the shape systemd
/// uses. Handling only a non-zero `attr_set` would clear both modelled atime
/// flags there and land on strictatime instead.
fn mount_attr_set_clr(attr_set: u64, attr_clr: u64) -> (MountFlags, MountFlags) {
    let mut set = mount_attr_to_flags(attr_set as u32);
    let mut clr = mount_attr_to_flags(attr_clr as u32);
    if attr_clr & MOUNT_ATTR__ATIME != 0 {
        // Both existing selections go, then the requested one is set. A
        // partially cleared group is EINVAL, which `check_mount_attr_bits` has
        // already rejected by only admitting `MOUNT_ATTR__ATIME` as a whole.
        clr |= MountFlags::NOATIME | MountFlags::RELATIME;
        if attr_set & MOUNT_ATTR_NOATIME != 0 {
            set |= MountFlags::NOATIME;
        } else if attr_set & MOUNT_ATTR_STRICTATIME == 0 {
            set |= MountFlags::RELATIME;
        }
    }
    if attr_set & MOUNT_ATTR_NODIRATIME != 0 {
        set |= MountFlags::NODIRATIME;
    }
    if attr_clr & MOUNT_ATTR_NODIRATIME != 0 {
        clr |= MountFlags::NODIRATIME;
    }
    (set, clr)
}

/// Applies `mount_setattr`'s `propagation` field to `target`.
///
/// Only one of the four flags can be set per call (`build_mount_kattr` rejects
/// a combination), and KVFS models no shared propagation groups, so each of
/// these is a validated idempotent operation. A request with no propagation
/// flag changes nothing.
fn apply_mount_propagation(
    namespace: &MntNamespace,
    target: &Path,
    propagation: u64,
) -> KResult<()> {
    use linux_raw_sys::general::{MS_PRIVATE, MS_SHARED, MS_SLAVE, MS_UNBINDABLE};
    let propagation = propagation as u32;
    if propagation & MS_SHARED != 0 {
        namespace.make_mount_shared(target)?;
    } else if propagation & MS_PRIVATE != 0 {
        namespace.make_mount_private(target)?;
    } else if propagation & MS_SLAVE != 0 {
        namespace.make_mount_slave(target)?;
    } else if propagation & MS_UNBINDABLE != 0 {
        namespace.make_mount_unbindable(target)?;
    }
    Ok(())
}

/// Changes mount attributes (`mount_setattr(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it checks the caller's credentials, reads
/// its fd table (`AT_EMPTY_PATH`) and mount namespace. An IRQ handler, a
/// softirq or a kernel task without a current process cannot call it. It may
/// sleep while resolving the target path and applying attributes, so callers
/// must hold no spinlock and no IRQ-disable or preempt-disable guard. The
/// namespace registry lock is taken and released inside the call; a caller must
/// not already hold it (nor the inode locks of the resolved path) on entry.
pub fn sys_mount_setattr(
    dfd: c_int,
    path: UserConstPtr<c_char>,
    flags: u32,
    attr: UserConstPtr<c_void>,
    size: usize,
) -> KResult<isize> {
    // Probes (e.g. systemd mount_new_api_supported) pass size=0 / attr=NULL /
    // path=NULL. Report ENOSYS before touching user memory so callers fall back
    // to the classic mount API instead of seeing a fatal bad-address error.
    if size < 16 || attr.is_null() {
        return Err(KError::Unsupported);
    }
    // Linux `mount_setattr` admits the four path-resolution flags only, and
    // bounds the structure size before reading it: `usize > PAGE_SIZE` is
    // `E2BIG`, anything below the published struct is `EINVAL`.
    if flags & !(AT_EMPTY_PATH | AT_RECURSIVE | 0x100 | 0x800) != 0 {
        return Err(KError::InvalidInput);
    }
    if size > memaddr::PAGE_SIZE_4K {
        return Err(KError::ArgumentListTooLong);
    }
    if size < MOUNT_ATTR_SIZE_VER0 {
        return Err(KError::InvalidInput);
    }
    // Changing a mount's attributes is a mount operation: `may_mount()` gates
    // it exactly as it gates every other entry point here.
    let cred = kprocess::current_cred();
    require_mount_privilege(&cred)?;
    let path = path.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_mount_setattr <= dfd: {dfd}, path: {path:?}, flags: {flags:#x}");
    let base = attr.as_ptr() as usize;
    let attr_set = UserConstPtr::<u64>::from(base).read_vm()?;
    let attr_clr = UserConstPtr::<u64>::from(base + 8).read_vm()?;
    // `struct mount_attr` carries two more fields. `propagation` is honoured
    // through the four `MntNamespace` propagation entry points; `userns_fd` is
    // only meaningful with `MOUNT_ATTR_IDMAP` and idmapped mounts are not
    // implemented, so a non-zero descriptor is reported unsupported rather than
    // silently ignored.
    let propagation = UserConstPtr::<u64>::from(base + 16).read_vm()?;
    // Read so the whole published struct is known to be accessible, the way
    // `copy_struct_from_user` faults on an unreadable page even for a field it
    // then ignores. Linux only consults the value when IDMAP is requested.
    let _userns_fd = UserConstPtr::<u64>::from(base + 24).read_vm()?;
    // `copy_struct_from_user` handles a larger structure by requiring the bytes
    // this kernel does not know to be zero: a newer caller's extension must not
    // be silently ignored, so a non-zero tail is `E2BIG` rather than success.
    check_mount_attr_tail(base, size)?;
    // `build_mount_kattr` rejects an attr bit the ABI does not define, so an
    // unknown bit is an error rather than a silent no-op.
    check_mount_attr_bits(attr_set)?;
    check_mount_attr_bits(attr_clr)?;
    check_mount_attr_atime(attr_set, attr_clr)?;
    // `build_mount_kattr`: only the four propagation flags are defined, and at
    // most one may be requested.
    const PROPAGATION_FLAGS: u64 = linux_raw_sys::general::MS_SHARED as u64
        | linux_raw_sys::general::MS_PRIVATE as u64
        | linux_raw_sys::general::MS_SLAVE as u64
        | linux_raw_sys::general::MS_UNBINDABLE as u64;
    if propagation & !PROPAGATION_FLAGS != 0 || (propagation & PROPAGATION_FLAGS).count_ones() > 1 {
        return Err(KError::InvalidInput);
    }
    // `build_mount_idmapped` decides an idmap request from the attribute word,
    // not from `userns_fd`: clearing an idmap is `EINVAL` ("we currently do not
    // support clearing an idmapped mount"), and setting one resolves the
    // descriptor, whose absence is `EBADF`. Accepting `attr_set = IDMAP` with
    // `userns_fd = 0` returned success for a request that changed nothing.
    const MOUNT_ATTR_IDMAP: u64 = 0x10_0000;
    if attr_clr & MOUNT_ATTR_IDMAP != 0 {
        return Err(KError::InvalidInput);
    }
    if attr_set & MOUNT_ATTR_IDMAP != 0 {
        // Idmapped mounts are not implemented, so the descriptor can never be
        // honoured; report the errno the reference kernel uses for a descriptor
        // that does not resolve, rather than claiming success.
        return Err(KError::BadFileDescriptor);
    }
    // `build_mount_idmapped` returns 0 without looking at `userns_fd` when
    // neither attribute word names IDMAP, so a stray descriptor is ignored
    // rather than refused: a caller reusing a struct must not be failed for a
    // field the request does not use (`userns_fd` stays unread past this).
    let (set_flags, clr_flags) = mount_attr_set_clr(attr_set, attr_clr);
    let recursive = flags & AT_RECURSIVE != 0;
    // AT_EMPTY_PATH on an open_tree/fsmount fd operates on that mount directly
    // rather than resolving a pathname.
    if path.is_empty()
        && flags & AT_EMPTY_PATH != 0
        && let Ok(file) = kprocess::current_files().get_file(dfd)
        && let Ok(mfd) = mount_fd(&file)
    {
        let mount = mfd.mount.lock().clone().ok_or(KError::InvalidInput)?;
        let namespace = kprocess::current_user_process().mnt_ns()?;
        namespace.set_mount_attributes_on(&mount, set_flags, clr_flags, recursive)?;
        // The fd may name a detached mount (`fsmount`), which is not in the
        // namespace registry yet, so this uses the registration-free form.
        if propagation != 0 {
            namespace.set_mount_propagation(&mount.root_path())?;
        }
        return Ok(0);
    }
    let filename = Filename::new(path.as_str());
    // The flag word admits `AT_SYMLINK_NOFOLLOW`/`AT_NO_AUTOMOUNT`, so the walk
    // has to honour them: following a symlink here would re-attribute a
    // different mount than the caller named.
    let mut lookup = LookupFlags::follow();
    if flags & 0x100 != 0 {
        lookup = LookupFlags::no_follow();
    }
    // `AT_EMPTY_PATH` with a non-mount fd still names that fd's own path, which
    // Linux resolves by passing `LOOKUP_EMPTY` to `user_path_at`. Without this
    // the empty path fell through to a pathname lookup and answered ENOENT.
    if flags & AT_EMPTY_PATH != 0 {
        lookup |= LookupFlags::EMPTY_PATH;
    }
    // `Stat` rather than `Open`: `mount_setattr` re-attributes a mount and never
    // opens the target, and `Open` rejects a non-directory, which returned
    // ENOTDIR for `AT_EMPTY_PATH` on an fd naming a file. Linux's `user_path_at`
    // performs no such check.
    let target = with_fs_at(dfd, &filename, |fs| {
        filename.lookup_at(fs.root(), fs.pwd(), LookupIntent::Stat, lookup, &cred)
    })?;
    let namespace = kprocess::current_user_process().mnt_ns()?;
    namespace.set_mount_attributes(&target, set_flags, clr_flags, recursive)?;
    if propagation != 0 {
        apply_mount_propagation(&namespace, &target, propagation)?;
    }
    Ok(0)
}

unittest::path_mod_test!(tests, "../ut-tests/mount_api.rs");
