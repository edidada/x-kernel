// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The firmware node snapshot: what enumeration read from the device tree
//! (or ACPI), reachable from every device regardless of bus.
//!
//! This used to live inside `PlatformBusData`, which made `of_property_read_*`
//! reachable only where the platform bus was statically known and forced
//! every reader through `dev.info().bus_data().firmware`. Firmware facts are true of
//! the DEVICE, not of the bus: they are a field of the descriptor now, and the
//! bus data is pure bus identity.

use alloc::vec::Vec;

/// One firmware node property, eagerly typed when the byte encoding is
/// unambiguous (`of_property_read_u32`-style views come from these).
///
/// Multi-cell payloads stay [`FirmwareProp::Bytes`] — length alone cannot
/// prove list intent; consumers reinterpret with the rules the
/// device-tree specification states for the property they know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareProp {
    /// Zero-length property — presence is the information.
    Empty,
    /// One NUL-terminated string.
    Str(&'static str),
    /// NUL-separated string list.
    StrList(Vec<&'static str>),
    /// One big-endian u32 cell.
    U32(u32),
    /// One big-endian u64 value.
    U64(u64),
    /// Everything else (raw cells, mixed payloads).
    Bytes(&'static [u8]),
}

impl FirmwareProp {
    /// Reinterpret as one u32 cell; `None` unless the encoding is exactly
    /// one 4-byte cell.
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::U32(value) => Some(*value),
            _ => None,
        }
    }

    /// Reinterpret a raw-cells payload as a big-endian u32 list. `None`
    /// unless the payload is whole 4-byte cells — the parse never
    /// guesses at list intent.
    pub fn u32_list(&self) -> Option<alloc::vec::Vec<u32>> {
        let bytes = match self {
            Self::Bytes(bytes) => *bytes,
            _ => return None,
        };
        if bytes.len() % core::mem::size_of::<u32>() != 0 {
            return None;
        }
        Some(
            bytes
                .chunks_exact(4)
                .map(|cell| u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
                .collect(),
        )
    }
}

/// The firmware node's snapshot: the facts a driver can read at probe
/// time WITHOUT re-walking firmware.
#[derive(Debug, Clone)]
pub struct FirmwareNodeInfo {
    /// DT `phandle` (also used as the clock [`ProviderId`](kclk)).
    pub phandle: Option<u32>,
    /// `clock-frequency` property, in Hz (`fixed-clock` nodes).
    pub clock_frequency: Option<u32>,
    /// `#clock-cells`; `Some(_)` marks the node as a clock provider.
    pub clock_cells: Option<u32>,
    /// First `clock-output-names` entry, if present.
    pub clock_output_name: Option<&'static str>,
    /// The node's own properties, firmware order, as raw payloads. Typing
    /// happens on read ([`FirmwareNodeInfo::prop`]) with the device-tree's
    /// one disambiguation rule — the parser emits bytes, the model decides
    /// what a property *is*.
    pub properties: Vec<(&'static str, &'static [u8])>,
}

impl FirmwareNodeInfo {
    /// An empty snapshot (a node with no extractable facts).
    pub const EMPTY: Self = Self {
        phandle: None,
        clock_frequency: None,
        clock_cells: None,
        clock_output_name: None,
        properties: Vec::new(),
    };

    /// Look up one property by name and type it.
    pub fn prop(&self, name: &str) -> Option<FirmwareProp> {
        self.properties
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| decode_prop(value))
    }
}

/// Type one firmware property payload.
///
/// The device-tree disambiguation rule, applied once: a payload that ends
/// in NUL is a string or string list; otherwise it is cells (4 bytes →
/// u32, 8 bytes → u64, anything else → raw bytes). Properties whose cells
/// happen to look like strings are reinterpreted by the caller through
/// [`FirmwareProp::u32_list`] — which is also how Linux resolves typing.
fn decode_prop(value: &'static [u8]) -> FirmwareProp {
    if value.is_empty() {
        return FirmwareProp::Empty;
    }
    if value[value.len() - 1] == 0
        && let Some(list) = decode_string_list(value)
    {
        return if list.len() == 1 {
            FirmwareProp::Str(list[0])
        } else {
            FirmwareProp::StrList(list)
        };
    }
    match value.len() {
        4 => FirmwareProp::U32(u32::from_be_bytes([value[0], value[1], value[2], value[3]])),
        8 => {
            let high = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
            let low = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
            FirmwareProp::U64((high as u64) << 32 | low as u64)
        }
        _ => FirmwareProp::Bytes(value),
    }
}

fn decode_string_list(value: &'static [u8]) -> Option<alloc::vec::Vec<&'static str>> {
    let mut out = alloc::vec::Vec::new();
    let mut rest = value;
    loop {
        let Some(pos) = rest.iter().position(|&byte| byte == 0) else {
            if rest.is_empty() {
                break;
            }
            return None;
        };
        out.push(core::str::from_utf8(&rest[..pos]).ok()?);
        rest = &rest[pos + 1..];
    }
    Some(out)
}
