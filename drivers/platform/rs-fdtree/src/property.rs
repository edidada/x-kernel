// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Strict, allocation-free property reads shared by DT binding interpreters.

use crate::{FdtNode, NodeProperty};

/// Failure to read a complete property with the requested shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyError {
    /// No property has this name.
    Missing,
    /// More than one property has this name on the node.
    Duplicate,
    /// The bytes are not a complete value of the requested encoding.
    InvalidEncoding,
    /// A list was supplied where a single value was required.
    WrongType,
    /// The output is too short; it has not been modified.
    BufferTooSmall {
        /// Required number of u32 cells, not bytes.
        required: usize,
    },
}

impl<'a> NodeProperty<'a> {
    /// Read one NUL-terminated UTF-8 string, rejecting additional strings.
    pub fn read_string(self) -> Result<&'a str, PropertyError> {
        let text = self.string_payload()?;
        if text.contains('\0') {
            return Err(PropertyError::WrongType);
        }
        Ok(text)
    }

    /// Validate the entire string list before returning its borrowed iterator.
    ///
    /// Empty string elements are preserved; binding-specific readers decide
    /// whether they are permitted. An empty byte property is not a string list.
    pub fn read_strings(self) -> Result<impl Iterator<Item = &'a str>, PropertyError> {
        Ok(self.string_payload()?.split('\0'))
    }

    /// Read exactly one big-endian u32 cell, never silently taking a prefix.
    pub fn read_u32(self) -> Result<u32, PropertyError> {
        if !self.value.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidEncoding);
        }
        let bytes = self
            .value
            .try_into()
            .map_err(|_| PropertyError::WrongType)?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// Read every cell; malformed input or short output leaves output unchanged.
    ///
    /// Empty properties are valid empty arrays. Extra output cells are untouched.
    pub fn read_u32_array(self, output: &mut [u32]) -> Result<usize, PropertyError> {
        if !self.value.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidEncoding);
        }
        let required = self.value.len() / 4;
        if output.len() < required {
            return Err(PropertyError::BufferTooSmall { required });
        }
        for (value, bytes) in output.iter_mut().zip(self.value.chunks_exact(4)) {
            *value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Ok(required)
    }

    fn string_payload(self) -> Result<&'a str, PropertyError> {
        let bytes = self
            .value
            .strip_suffix(&[0])
            .ok_or(PropertyError::InvalidEncoding)?;
        core::str::from_utf8(bytes).map_err(|_| PropertyError::InvalidEncoding)
    }
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Read a uniquely named property, distinguishing absence from duplication.
    ///
    /// Scans this node's properties only; no tree index or allocation is required.
    pub fn property_strict(self, name: &str) -> Result<NodeProperty<'a>, PropertyError> {
        let mut matches = self.properties().filter(|property| property.name == name);
        let property = matches.next().ok_or(PropertyError::Missing)?;
        if matches.next().is_some() {
            return Err(PropertyError::Duplicate);
        }
        Ok(property)
    }

    /// Read a uniquely named, single NUL-terminated UTF-8 string.
    pub fn read_string(self, name: &str) -> Result<&'a str, PropertyError> {
        self.property_strict(name)?.read_string()
    }

    /// Validate all string elements before publishing a borrowed iterator.
    pub fn read_strings(self, name: &str) -> Result<impl Iterator<Item = &'a str>, PropertyError> {
        self.property_strict(name)?.read_strings()
    }

    /// Read exactly one cell, distinguishing missing, malformed and list values.
    pub fn read_u32(self, name: &str) -> Result<u32, PropertyError> {
        self.property_strict(name)?.read_u32()
    }

    /// Read all cells, leaving output unchanged when the property cannot be read.
    pub fn read_u32_array(self, name: &str, output: &mut [u32]) -> Result<usize, PropertyError> {
        self.property_strict(name)?.read_u32_array(output)
    }
}
