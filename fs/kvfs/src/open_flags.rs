// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File open flag normalization.

use linux_raw_sys::general::{
    FASYNC, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_DSYNC, O_EXCL,
    O_LARGEFILE, O_NOATIME, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR, O_SYNC,
    O_TRUNC, O_WRONLY,
};

use crate::{LookupFlags, NodePermission, NodeType, Permission, Umode, VfsError, VfsResult};

bitflags::bitflags! {
    /// Normalized Linux `O_*` bits used inside the VFS open path.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct OpenFlags: u32 {
        /// Open for writing only.
        const WRITE_ONLY = O_WRONLY;
        /// Open for reading and writing.
        const READ_WRITE = O_RDWR;
        /// Create the file when it does not exist.
        const CREATE = O_CREAT;
        /// Require exclusive creation.
        const EXCLUSIVE = O_EXCL;
        /// Do not assign a controlling terminal.
        const NO_CONTROLLING_TTY = O_NOCTTY;
        /// Truncate a regular file after opening it.
        const TRUNCATE = O_TRUNC;
        /// Append writes to the end of the file.
        const APPEND = O_APPEND;
        /// Use nonblocking I/O semantics.
        const NONBLOCK = O_NONBLOCK;
        /// Request data-integrity synchronized writes.
        const DSYNC = O_DSYNC;
        /// Enable signal-driven I/O.
        const ASYNC = FASYNC;
        /// Request direct I/O.
        const DIRECT = O_DIRECT;
        /// Allow large-file offsets.
        const LARGE_FILE = O_LARGEFILE;
        /// Require the opened object to be a directory.
        const DIRECTORY = O_DIRECTORY;
        /// Do not follow the final symbolic link.
        const NO_FOLLOW = O_NOFOLLOW;
        /// Do not update access time.
        const NO_ATIME = O_NOATIME;
        /// Close the descriptor during exec.
        const CLOSE_ON_EXEC = O_CLOEXEC;
        /// Request file-integrity synchronized writes.
        const SYNC = O_SYNC;
        /// Open only a path reference.
        const PATH = O_PATH;
    }

    /// Namei intent bits derived from open flags.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct OpenIntent: u32 {
        const OPEN = 1 << 16;
        const CREATE = 1 << 17;
        const EXCL = 1 << 18;
    }
}

const O_PATH_FLAGS: OpenFlags = OpenFlags::DIRECTORY
    .union(OpenFlags::NO_FOLLOW)
    .union(OpenFlags::PATH)
    .union(OpenFlags::CLOSE_ON_EXEC);
const VALID_OPEN_FLAGS: u32 = OpenFlags::all().bits();

fn is_create_like_open(flags: OpenFlags) -> bool {
    flags.contains(OpenFlags::CREATE)
}

/// `RESOLVE_NO_XDEV` (`uapi/linux/openat2.h`).
pub(crate) const RESOLVE_NO_XDEV: u64 = 0x01;
/// `RESOLVE_NO_MAGICLINKS` (`uapi/linux/openat2.h`).
pub(crate) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
/// `RESOLVE_NO_SYMLINKS` (`uapi/linux/openat2.h`).
pub(crate) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
/// `RESOLVE_BENEATH` (`uapi/linux/openat2.h`).
pub const RESOLVE_BENEATH: u64 = 0x08;
/// `RESOLVE_IN_ROOT` (`uapi/linux/openat2.h`).
pub const RESOLVE_IN_ROOT: u64 = 0x10;
/// `RESOLVE_CACHED` (`uapi/linux/openat2.h`).
pub(crate) const RESOLVE_CACHED: u64 = 0x20;
/// Every resolve bit this kernel understands (`openat2` `VALID_RESOLVE_FLAGS`).
const VALID_RESOLVE_FLAGS: u64 = RESOLVE_NO_XDEV
    | RESOLVE_NO_MAGICLINKS
    | RESOLVE_NO_SYMLINKS
    | RESOLVE_BENEATH
    | RESOLVE_IN_ROOT
    | RESOLVE_CACHED;

/// The `flags`/`mode`/`resolve` triple of `openat2(2)`'s `struct open_how`,
/// split out so the syscall layer does not need to know the internal layout.
#[derive(Clone, Copy, Debug)]
pub struct Openat2How {
    /// `open_how.flags` (`O_*` bits).
    pub flags: u64,
    /// `open_how.mode` (creation permission bits).
    pub mode: u64,
    /// `open_how.resolve` (`RESOLVE_*` bits).
    pub resolve: u64,
}

/// Raw open arguments after legacy syscall cleanup.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenHow {
    pub(crate) flags: u64,
    pub(crate) mode: NodePermission,
    /// `openat2` resolve bits; always zero for legacy `open`/`openat`.
    pub(crate) resolve: u64,
}

impl OpenHow {
    /// Builds open arguments from legacy `open`/`openat` syscall inputs.
    pub(crate) fn from_legacy(flags: u32, mode: NodePermission) -> Self {
        let mut how = Self {
            flags: u64::from(flags & VALID_OPEN_FLAGS),
            mode,
            resolve: 0,
        };
        if OpenFlags::from_bits_retain(how.flags as u32).contains(OpenFlags::PATH) {
            how.flags &= u64::from(O_PATH_FLAGS.bits());
        }
        if !is_create_like_open(OpenFlags::from_bits_retain(how.flags as u32)) {
            how.mode = NodePermission::empty();
        }
        how
    }

    /// Validates raw `openat2(2)` arguments (`fs/open.c build_open_flags()`).
    ///
    /// `flags`, `mode` and `resolve` come straight from `struct open_how`.
    /// Unknown flag or resolve bits are `EINVAL`, a non-zero `mode` without
    /// `O_CREAT` is `EINVAL`, and the two subtree constraints
    /// (`RESOLVE_BENEATH` and `RESOLVE_IN_ROOT`) are mutually exclusive.
    pub(crate) fn from_openat2(flags: u64, mode: u64, resolve: u64) -> VfsResult<Self> {
        let flags_u32 = u32::try_from(flags).map_err(|_| VfsError::InvalidInput)?;
        if flags_u32 & !VALID_OPEN_FLAGS != 0 {
            return Err(VfsError::InvalidInput);
        }
        let mode_permission = u16::try_from(mode).map_err(|_| VfsError::InvalidInput)?;
        let mode_permission = NodePermission::from_bits_truncate(mode_permission);
        if !is_create_like_open(OpenFlags::from_bits_retain(flags_u32)) && mode != 0 {
            return Err(VfsError::InvalidInput);
        }
        if resolve & !VALID_RESOLVE_FLAGS != 0 {
            return Err(VfsError::InvalidInput);
        }
        if resolve & RESOLVE_BENEATH != 0 && resolve & RESOLVE_IN_ROOT != 0 {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            flags: flags & u64::from(VALID_OPEN_FLAGS),
            mode: mode_permission,
            resolve,
        })
    }

    /// Converts raw open arguments into normalized parameters for namei.
    pub(crate) fn into_open_params(self) -> VfsResult<OpenParams> {
        let raw_flags = u32::try_from(self.flags).map_err(|_| VfsError::InvalidInput)?;
        let mut acc_mode = AccMode::from_access_mode(raw_flags & O_ACCMODE);

        let mut flags = OpenFlags::from_bits(raw_flags).ok_or(VfsError::InvalidInput)?;

        flags.remove(OpenFlags::CLOSE_ON_EXEC);

        let mode = if is_create_like_open(flags) {
            Umode::new(NodeType::RegularFile, self.mode)
        } else {
            if !self.mode.is_empty() {
                return Err(VfsError::InvalidInput);
            }
            Umode::new(NodeType::RegularFile, NodePermission::empty())
        };

        if flags.contains(OpenFlags::DIRECTORY | OpenFlags::CREATE) {
            return Err(VfsError::InvalidInput);
        }

        if flags.contains(OpenFlags::PATH) {
            if !O_PATH_FLAGS.contains(flags) {
                return Err(VfsError::InvalidInput);
            }
            acc_mode = AccMode::empty();
        }

        if flags.contains(OpenFlags::SYNC) {
            flags.insert(OpenFlags::DSYNC);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            acc_mode.insert_write();
        }
        if flags.contains(OpenFlags::APPEND) {
            acc_mode.insert_append();
        }

        let mut intent = if flags.contains(OpenFlags::PATH) {
            OpenIntent::empty()
        } else {
            OpenIntent::OPEN
        };
        if flags.contains(OpenFlags::CREATE) {
            intent |= OpenIntent::CREATE;
            if flags.contains(OpenFlags::EXCLUSIVE) {
                intent |= OpenIntent::EXCL;
                flags.insert(OpenFlags::NO_FOLLOW);
            }
        }

        let mut lookup_flags = LookupFlags::empty();
        if flags.contains(OpenFlags::DIRECTORY) {
            lookup_flags |= LookupFlags::DIRECTORY;
        }
        if !flags.contains(OpenFlags::NO_FOLLOW) {
            lookup_flags |= LookupFlags::FOLLOW_FINAL;
        }
        // `openat2(2)` resolve constraints. `RESOLVE_IN_ROOT` needs no lookup
        // bit: the syscall layer passes the directory as the walk root, and
        // `Nameidata` already rebases absolute pathnames and stops ".." at the
        // root. `RESOLVE_CACHED` is a performance hint ("answer from cache or
        // fail with EAGAIN"); answering without the hint is semantically
        // correct, so it is accepted and ignored.
        if self.resolve & RESOLVE_NO_XDEV != 0 {
            lookup_flags |= LookupFlags::NO_XDEV;
        }
        if self.resolve & RESOLVE_NO_MAGICLINKS != 0 {
            lookup_flags |= LookupFlags::NO_MAGIC_LINKS;
        }
        if self.resolve & RESOLVE_NO_SYMLINKS != 0 {
            lookup_flags |= LookupFlags::NO_SYMLINKS;
        }
        if self.resolve & RESOLVE_BENEATH != 0 {
            lookup_flags |= LookupFlags::BENEATH;
        }

        Ok(OpenParams {
            flags,
            mode,
            acc_mode,
            intent,
            lookup_flags,
        })
    }
}

/// Open parameters consumed by namei.
#[derive(Debug, Clone)]
pub(crate) struct OpenParams {
    flags: OpenFlags,
    mode: Umode,
    acc_mode: AccMode,
    intent: OpenIntent,
    lookup_flags: LookupFlags,
}

impl OpenParams {
    pub(crate) const fn will_create(&self) -> bool {
        self.intent.contains(OpenIntent::CREATE)
    }

    pub(crate) const fn is_exclusive_create(&self) -> bool {
        self.intent.contains(OpenIntent::EXCL)
    }

    pub(crate) const fn mode(&self) -> Umode {
        self.mode
    }

    pub(crate) const fn lookup_flags(&self) -> LookupFlags {
        self.lookup_flags
    }

    pub(crate) const fn is_path(&self) -> bool {
        self.flags.contains(OpenFlags::PATH)
    }

    pub(crate) const fn file_flags(&self) -> OpenFlags {
        self.flags
    }

    pub(crate) fn may_open_args(&self, was_created: bool) -> (OpenFlags, AccMode) {
        let mut open_flags = self.flags;
        let mut acc_mode = self.acc_mode;
        if was_created {
            open_flags.remove(OpenFlags::TRUNCATE);
            acc_mode = AccMode::empty();
        }
        (open_flags, acc_mode)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccMode(Permission);

impl AccMode {
    pub(crate) const fn empty() -> Self {
        Self(Permission::empty())
    }

    fn from_access_mode(access_mode: u32) -> Self {
        match access_mode {
            O_RDONLY => Self(Permission::MAY_READ),
            O_WRONLY => Self(Permission::MAY_WRITE),
            O_RDWR => Self(Permission::MAY_READ | Permission::MAY_WRITE),
            _ => Self(Permission::MAY_READ | Permission::MAY_WRITE),
        }
    }

    fn insert_write(&mut self) {
        self.0 |= Permission::MAY_WRITE;
    }

    fn insert_append(&mut self) {
        self.0 |= Permission::MAY_APPEND;
    }

    pub(crate) const fn requires_write(self) -> bool {
        self.0.contains(Permission::MAY_WRITE)
    }

    pub(crate) const fn permission(self) -> Permission {
        self.0
    }
}

unittest::path_mod_test!(tests, "../ut-tests/open_flags.rs");
