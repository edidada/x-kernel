// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use core::fmt;

use strum::EnumCount;

/// The error kind type used by x-kernel.
///
/// Similar to [`std::io::ErrorKind`].
///
/// [`std::io::ErrorKind`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html
#[repr(i32)]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, EnumCount)]
pub enum KErrorKind {
    /// A socket address could not be bound because the address is already in use elsewhere.
    AddrInUse = 1,
    /// The socket is already connected.
    AlreadyConnected,
    /// An entity already exists, often a file.
    AlreadyExists,
    /// Program argument list too long.
    ArgumentListTooLong,
    /// Bad address.
    BadAddress,
    /// Bad file descriptor.
    BadFileDescriptor,
    /// Bad internal state.
    BadState,
    /// Broken pipe
    BrokenPipe,
    /// The connection was refused by the remote server.
    ConnectionRefused,
    /// The connection was reset by the remote server.
    ConnectionReset,
    /// Cross-device or cross-filesystem (hard) link or rename.
    CrossesDevices,
    /// A non-empty directory was specified where an empty directory was expected.
    DirectoryNotEmpty,
    /// Loop in the filesystem or IO subsystem; often, too many levels of
    /// symbolic links.
    FilesystemLoop,
    /// Illegal byte sequence.
    IllegalBytes,
    /// The operation was partially successful and needs to be checked later on
    /// due to not blocking.
    InProgress,
    /// This operation was interrupted.
    Interrupted,
    /// Data not valid for the operation were encountered.
    ///
    /// Unlike [`InvalidInput`], this typically means that the operation
    /// parameters were valid, however the error was caused by malformed
    /// input data.
    ///
    /// For example, a function that reads a file into a string will error with
    /// `InvalidData` if the file's contents are not valid UTF-8.
    ///
    /// [`InvalidInput`]: KErrorKind::InvalidInput
    InvalidData,
    /// Invalid executable format.
    InvalidExecutable,
    /// Invalid parameter/argument.
    InvalidInput,
    /// Input/output error.
    Io,
    /// The filesystem object is, unexpectedly, a directory.
    IsADirectory,
    /// Filename is too long.
    NameTooLong,
    /// Not enough space/cannot allocate memory.
    NoMemory,
    /// No such device.
    NoSuchDevice,
    /// No such process.
    NoSuchProcess,
    /// A filesystem object is, unexpectedly, not a directory.
    NotADirectory,
    /// The specified entity is not a socket.
    NotASocket,
    /// Not a typewriter.
    NotATty,
    /// The network operation failed because it was not connected yet.
    NotConnected,
    /// The requested entity is not found.
    NotFound,
    /// Operation not permitted.
    OperationNotPermitted,
    /// Operation not supported.
    OperationNotSupported,
    /// Result out of range.
    OutOfRange,
    /// The operation lacked the necessary privileges to complete.
    PermissionDenied,
    /// The filesystem or storage medium is read-only, but a write operation was attempted.
    ReadOnlyFilesystem,
    /// Device or resource is busy.
    ResourceBusy,
    /// The underlying storage (typically, a filesystem) is full.
    StorageFull,
    /// The I/O operation’s timeout expired, causing it to be canceled.
    TimedOut,
    /// The process has too many files open.
    TooManyOpenFiles,
    /// An error returned when an operation could not be completed because an
    /// "end of file" was reached prematurely.
    UnexpectedEof,
    /// This operation is unsupported or unimplemented.
    Unsupported,
    /// The operation needs to block to complete, but the blocking operation was
    /// requested to not occur.
    WouldBlock,
    /// An error returned when an operation could not be completed because a
    /// call to `write()` returned [`Ok(0)`](Ok).
    WriteZero,
    /// The connection was aborted.
    ConnectionAborted,
    /// File is too large for the filesystem or VFS limit.
    FileTooLarge,
}

impl KErrorKind {
    /// Returns the error description.
    pub fn as_str(&self) -> &'static str {
        use KErrorKind::*;
        match *self {
            AddrInUse => "Address in use",
            AlreadyConnected => "Already connected",
            AlreadyExists => "Entity already exists",
            ArgumentListTooLong => "Argument list too long",
            BadAddress => "Bad address",
            BadFileDescriptor => "Bad file descriptor",
            BadState => "Bad internal state",
            BrokenPipe => "Broken pipe",
            ConnectionAborted => "Connection aborted",
            ConnectionRefused => "Connection refused",
            ConnectionReset => "Connection reset",
            CrossesDevices => "Cross-device link or rename",
            DirectoryNotEmpty => "Directory not empty",
            FilesystemLoop => "Filesystem loop or indirection limit",
            IllegalBytes => "Illegal byte sequence",
            InProgress => "Operation in progress",
            Interrupted => "Operation interrupted",
            InvalidData => "Invalid data",
            InvalidExecutable => "Invalid executable format",
            InvalidInput => "Invalid input parameter",
            Io => "I/O error",
            IsADirectory => "Is a directory",
            NameTooLong => "Filename too long",
            NoMemory => "Out of memory",
            NoSuchDevice => "No such device",
            NoSuchProcess => "No such process",
            NotADirectory => "Not a directory",
            NotASocket => "Not a socket",
            NotATty => "Inappropriate ioctl for device",
            NotConnected => "Not connected",
            NotFound => "Entity not found",
            OperationNotPermitted => "Operation not permitted",
            OperationNotSupported => "Operation not supported",
            OutOfRange => "Result out of range",
            PermissionDenied => "Permission denied",
            ReadOnlyFilesystem => "Read-only filesystem",
            ResourceBusy => "Resource busy",
            StorageFull => "No storage space",
            TimedOut => "Timed out",
            TooManyOpenFiles => "Too many open files",
            UnexpectedEof => "Unexpected end of file",
            Unsupported => "Operation not supported",
            WouldBlock => "Operation would block",
            WriteZero => "Write zero",
            FileTooLarge => "File too large",
        }
    }

    /// Returns the error code value in `i32`.
    pub const fn code(self) -> i32 {
        self as i32
    }

    #[inline]
    const fn from_code(value: i32) -> Option<Self> {
        use KErrorKind::*;

        Some(match value {
            1 => AddrInUse,
            2 => AlreadyConnected,
            3 => AlreadyExists,
            4 => ArgumentListTooLong,
            5 => BadAddress,
            6 => BadFileDescriptor,
            7 => BadState,
            8 => BrokenPipe,
            9 => ConnectionRefused,
            10 => ConnectionReset,
            11 => CrossesDevices,
            12 => DirectoryNotEmpty,
            13 => FilesystemLoop,
            14 => IllegalBytes,
            15 => InProgress,
            16 => Interrupted,
            17 => InvalidData,
            18 => InvalidExecutable,
            19 => InvalidInput,
            20 => Io,
            21 => IsADirectory,
            22 => NameTooLong,
            23 => NoMemory,
            24 => NoSuchDevice,
            25 => NoSuchProcess,
            26 => NotADirectory,
            27 => NotASocket,
            28 => NotATty,
            29 => NotConnected,
            30 => NotFound,
            31 => OperationNotPermitted,
            32 => OperationNotSupported,
            33 => OutOfRange,
            34 => PermissionDenied,
            35 => ReadOnlyFilesystem,
            36 => ResourceBusy,
            37 => StorageFull,
            38 => TimedOut,
            39 => TooManyOpenFiles,
            40 => UnexpectedEof,
            41 => Unsupported,
            42 => WouldBlock,
            43 => WriteZero,
            44 => ConnectionAborted,
            45 => FileTooLarge,
            _ => return None,
        })
    }
}

impl TryFrom<i32> for KErrorKind {
    type Error = i32;

    #[inline]
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        KErrorKind::from_code(value).ok_or(value)
    }
}

impl fmt::Display for KErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The error type used by X-Kernel internals.
#[repr(transparent)]
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct KError(KErrorKind);

impl KError {
    const fn new(kind: KErrorKind) -> Self {
        Self(kind)
    }

    /// Returns the internal error code value.
    pub const fn code(self) -> i32 {
        self.0.code()
    }

    /// Returns this already-canonical internal error unchanged.
    pub const fn canonicalize(self) -> Self {
        self
    }

    /// Constructs an error from an internal error code.
    pub fn try_from_i32(value: i32) -> Result<Self, i32> {
        KErrorKind::try_from(value).map(Self).map_err(|_| value)
    }
}

impl From<KErrorKind> for KError {
    fn from(error: KErrorKind) -> Self {
        Self::new(error)
    }
}

impl From<KError> for KErrorKind {
    fn from(error: KError) -> Self {
        error.0
    }
}

impl fmt::Debug for KError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KErrorKind::{:?}", self.0)
    }
}

impl fmt::Display for KError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! kerror_consts {
    ($($name:ident),*) => {
        #[allow(non_upper_case_globals)]
        impl KError {
            $(
                #[doc = concat!("An [`KError`] with kind [`KErrorKind::", stringify!($name), "`].")]
                pub const $name: Self = Self::new(KErrorKind::$name);
            )*
        }
    };
}

kerror_consts!(
    AddrInUse,
    AlreadyConnected,
    AlreadyExists,
    ArgumentListTooLong,
    BadAddress,
    BadFileDescriptor,
    BadState,
    BrokenPipe,
    ConnectionRefused,
    ConnectionReset,
    CrossesDevices,
    DirectoryNotEmpty,
    FilesystemLoop,
    IllegalBytes,
    InProgress,
    Interrupted,
    InvalidData,
    InvalidExecutable,
    InvalidInput,
    Io,
    IsADirectory,
    NameTooLong,
    NoMemory,
    NoSuchDevice,
    NoSuchProcess,
    NotADirectory,
    NotASocket,
    NotATty,
    NotConnected,
    NotFound,
    OperationNotPermitted,
    OperationNotSupported,
    OutOfRange,
    PermissionDenied,
    ReadOnlyFilesystem,
    ResourceBusy,
    StorageFull,
    TimedOut,
    TooManyOpenFiles,
    UnexpectedEof,
    Unsupported,
    WouldBlock,
    WriteZero,
    ConnectionAborted,
    FileTooLarge
);

/// A specialized [`Result`] type with [`KError`] as the error type.
pub type KResult<T = ()> = Result<T, KError>;

/// Convenience method to construct an [`KError`] type while printing a warning
/// message.
///
/// # Examples
///
/// ```
/// # use kerrno::{k_err_type, KError};
/// #
/// // Also print "[KError::AlreadyExists]" if the `log` crate is enabled.
/// assert_eq!(k_err_type!(AlreadyExists), KError::AlreadyExists,);
///
/// // Also print "[KError::BadAddress] the address is 0!" if the `log` crate
/// // is enabled.
/// assert_eq!(
///     k_err_type!(BadAddress, "the address is 0!"),
///     KError::BadAddress,
/// );
/// ```
#[macro_export]
macro_rules! k_err_type {
    ($err:ident) => {{
        use $crate::KErrorKind::*;
        let err = $crate::KError::from($err);
        $crate::__priv::warn!("[{:?}]", err);
        err
    }};
    ($err:ident, $msg:expr) => {{
        use $crate::KErrorKind::*;
        let err = $crate::KError::from($err);
        $crate::__priv::warn!("[{:?}] {}", err, $msg);
        err
    }};
}

/// Ensure a condition is true. If it is not, return from the function
/// with an error.
///
/// ## Examples
///
/// ```rust
/// # use kerrno::{ensure, k_err, KError, KResult};
///
/// fn example(user_id: i32) -> KResult {
///     ensure!(user_id > 0, k_err!(InvalidInput));
///     // After this point, we know that `user_id` is positive.
///     let user_id = user_id as u32;
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! ensure {
    ($predicate:expr, $context_selector:expr $(,)?) => {
        if !$predicate {
            return $context_selector;
        }
    };
}

/// Convenience method to construct an [`Err(KError)`] type while printing a
/// warning message.
///
/// # Examples
///
/// ```
/// # use kerrno::{k_err, KResult, KError};
/// #
/// // Also print "[KError::AlreadyExists]" if the `log` crate is enabled.
/// assert_eq!(
///     k_err!(AlreadyExists),
///     KResult::<()>::Err(KError::AlreadyExists),
/// );
///
/// // Also print "[KError::BadAddress] the address is 0!" if the `log` crate is enabled.
/// assert_eq!(
///     k_err!(BadAddress, "the address is 0!"),
///     KResult::<()>::Err(KError::BadAddress),
/// );
/// ```
/// [`Err(KError)`]: Err
#[macro_export]
macro_rules! k_err {
    ($err:ident) => {
        Err($crate::k_err_type!($err))
    };
    ($err:ident, $msg:expr) => {
        Err($crate::k_err_type!($err, $msg))
    };
}

/// Throws an error of type [`KError`] with the given error code, optionally
/// with a message.
#[macro_export]
macro_rules! k_bail {
    ($($t:tt)*) => {
        return $crate::k_err!($($t)*);
    };
}

#[doc(hidden)]
pub mod __priv {
    pub use log::warn;
}

#[cfg(test)]
mod tests {
    use strum::EnumCount;

    use crate::{KError, KErrorKind};

    #[test]
    fn internal_codes_round_trip() {
        let max_code = KErrorKind::COUNT as i32;
        assert_eq!(max_code, KError::FileTooLarge.code());
        assert_eq!(Ok(KError::AddrInUse), KError::try_from_i32(1));
        assert_eq!(Ok(KError::FileTooLarge), KError::try_from_i32(max_code));
        assert_eq!(Err(0), KError::try_from_i32(0));
        assert_eq!(Err(-1), KError::try_from_i32(-1));
        assert_eq!(Err(max_code + 1), KError::try_from_i32(max_code + 1));
    }
}
