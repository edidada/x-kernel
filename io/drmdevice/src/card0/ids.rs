// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed resource identifiers for the virtual DRM card.
//!
//! Userspace sees bare `u32` handles for every DRM object family, which makes
//! accidental cross-family mixing (a framebuffer id where a dumb handle is
//! expected, and so on) unrepresentable inside the driver. The wire format
//! stays `u32`: conversion happens exactly once, at the ioctl boundary,
//! through `from_wire`/`wire`.

use super::*;

/// A framebuffer registered through `ADDFB`/`ADDFB2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FbId(pub(crate) u32);

impl FbId {
    pub(crate) fn from_wire(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) const fn wire(self) -> u32 {
        self.0
    }
}

/// A GEM dumb buffer allocated through `CREATE_DUMB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DumbHandle(pub(crate) u32);

impl DumbHandle {
    pub(crate) fn from_wire(raw: u32) -> Self {
        Self(raw)
    }
}

/// A property-value blob registered through `MODE_CREATE_BLOB` (or the
/// driver's own system blobs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlobId(pub(crate) u32);

impl BlobId {
    pub(crate) fn from_wire(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) const fn wire(self) -> u32 {
        self.0
    }
}

/// The KMS object classes this driver exposes, one instance each.
///
/// Replaces the raw `DRM_MODE_OBJECT_*` wire words inside the driver:
/// `from_wire` decodes the word that travels in `MODE_OBJ_SETPROPERTY`,
/// `of_resource_id` classifies the fixed singleton ids, and either way a
/// plane property can never be applied to a CRTC by passing the wrong `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrmObjectType {
    Crtc,
    Connector,
    Plane,
}

impl DrmObjectType {
    pub(crate) fn from_wire(wire: u32) -> Option<Self> {
        match wire {
            DRM_MODE_OBJECT_CRTC => Some(Self::Crtc),
            DRM_MODE_OBJECT_CONNECTOR => Some(Self::Connector),
            DRM_MODE_OBJECT_PLANE => Some(Self::Plane),
            _ => None,
        }
    }

    pub(crate) fn of_resource_id(id: u32) -> Option<Self> {
        match id {
            CRTC_ID => Some(Self::Crtc),
            CONNECTOR_ID => Some(Self::Connector),
            PLANE_ID => Some(Self::Plane),
            _ => None,
        }
    }
}
