// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error vocabulary of the device model.
//!
//! There are exactly two ways a probe can fail:
//!
//! * [`ProbeError::Deferred`] — a dependency is not available yet; the device
//!   model parks the descriptor and retries when the dependency is published
//!   (the `-EPROBE_DEFER` analogue).
//! * [`ProbeError::Fatal`] — this driver cannot drive this device; the model
//!   tries the next candidate driver.
//!
//! Driver-visible service calls report [`RuntimeError`], a small errno-like
//! vocabulary that is deliberately separate from probe control flow so the
//! two can never be conflated the way `should_retry()`-style overloads do.

use core::fmt;

/// What a probe is waiting for when it returns [`ProbeError::Deferred`].
///
/// The source identifies the publisher whose registration will wake the
/// parked device, so wakeups are exact instead of "retry everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepSource {
    pub kind: DepKind,
    /// Opaque publisher id: DT phandle, provider id, or device id.
    pub id: u64,
}

impl DepSource {
    pub const fn new(kind: DepKind, id: u64) -> Self {
        Self { kind, id }
    }

    pub const fn clk(provider_phandle: u64) -> Self {
        Self::new(DepKind::Clk, provider_phandle)
    }

    pub const fn device(id: u64) -> Self {
        Self::new(DepKind::Device, id)
    }
}

/// The kind of dependency a deferred probe can wait on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DepKind {
    /// A clock provider (`clk_get` target).
    Clk,
    /// Another device being bound first.
    Device,
    /// Anything else; matched by `id` only.
    Other,
}

impl DepKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clk => "clk",
            Self::Device => "device",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for DepSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.id)
    }
}

/// Control-flow result of a probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// Retry this device once the dependency publishes itself.
    Deferred(DepSource),
    /// This driver gives up on this device; try the next candidate.
    Fatal(RuntimeError),
}

impl ProbeError {
    /// Convenience for returning from a probe.
    pub const fn defer(dep: DepSource) -> Self {
        Self::Deferred(dep)
    }
}

impl From<RuntimeError> for ProbeError {
    fn from(err: RuntimeError) -> Self {
        Self::Fatal(err)
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deferred(dep) => write!(f, "waiting for dependency {dep}"),
            Self::Fatal(err) => write!(f, "{err}"),
        }
    }
}

/// Errno-like service error. Kept minimal and stable; subsystems map their
/// rich errors into these categories at the service boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeError {
    /// Management entry failed before executing a lifecycle operation.
    Management(crate::DeviceManagementError),
    Invalid,
    NoMemory,
    Busy,
    Io,
    BadState,
    Unsupported,
    AlreadyExists,
    NotFound,
}

impl RuntimeError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Management(crate::DeviceManagementError::InvalidContext) => {
                "invalid management context"
            }
            Self::Management(crate::DeviceManagementError::Reentrant) => {
                "recursive device management"
            }
            Self::Invalid => "invalid argument",
            Self::NoMemory => "out of memory",
            Self::Busy => "resource busy",
            Self::Io => "i/o error",
            Self::BadState => "bad state",
            Self::Unsupported => "unsupported operation",
            Self::AlreadyExists => "already exists",
            Self::NotFound => "not found",
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<crate::DeviceManagementError> for RuntimeError {
    fn from(error: crate::DeviceManagementError) -> Self {
        Self::Management(error)
    }
}

/// Convert OS capability failures into device-service errors.
///
/// `ResError` is the host capability vocabulary and `RuntimeError` is what a
/// driver sees; without a single `From` every host bridge re-derives the
/// mapping (and the classifications drift). Hosts call
/// `RuntimeError::from(err)` / `.map_err(RuntimeError::from)`.
impl From<driver_os_api::ResError> for crate::error::RuntimeError {
    fn from(err: driver_os_api::ResError) -> Self {
        use driver_os_api::ResError;
        match err {
            ResError::InvalidResource => Self::Invalid,
            ResError::MappingFailed | ResError::Io => Self::Io,
            ResError::NoMemory => Self::NoMemory,
            ResError::Busy => Self::Busy,
            ResError::Unsupported => Self::Unsupported,
            ResError::BadState => Self::BadState,
            ResError::AlreadyExists => Self::AlreadyExists,
            ResError::NotFound => Self::NotFound,
        }
    }
}

impl From<driver_os_api::ResError> for ProbeError {
    fn from(error: driver_os_api::ResError) -> Self {
        Self::Fatal(RuntimeError::from(error))
    }
}
