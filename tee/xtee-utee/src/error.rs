// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use num_enum::{FromPrimitive, IntoPrimitive};
use rust_utee::tee_api_defines::*;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug, IntoPrimitive, FromPrimitive)]
#[repr(u32)]
pub enum Error {
    #[error("TEE error: object is corrupt")]
    CorruptObject = TEE_ERROR_CORRUPT_OBJECT,
    #[error("TEE error: persistentobject is corrupt")]
    CorruptObject2 = TEE_ERROR_CORRUPT_OBJECT_2,
    #[error("TEE error: object storage is currently unavailable")]
    StorageNotAvailable = TEE_ERROR_STORAGE_NOT_AVAILABLE,
    #[error("TEE error: persistent object storage is unavailable")]
    StorageNotAvailable2 = TEE_ERROR_STORAGE_NOT_AVAILABLE_2,
    #[error("TEE error: non-specific error occurred during operation")]
    Generic = TEE_ERROR_GENERIC,
    #[error("TEE error: access denied, insufficient permissions")]
    AccessDenied = TEE_ERROR_ACCESS_DENIED,
    #[error("TEE error: operation was cancelled")]
    Cancel = TEE_ERROR_CANCEL,
    #[error("TEE error: access conflict, concurrent access to the resource is not allowed")]
    AccessConflict = TEE_ERROR_ACCESS_CONFLICT,
    #[error("TEE error: too much data provided for the requested operation")]
    ExcessData = TEE_ERROR_EXCESS_DATA,
    #[error("TEE error: input data format is incorrect or malformed")]
    BadFormat = TEE_ERROR_BAD_FORMAT,
    #[error("TEE error: one or more parameters are invalid or out of range")]
    BadParameters = TEE_ERROR_BAD_PARAMETERS,
    #[error("TEE error: operation is not valid in the current state")]
    BadState = TEE_ERROR_BAD_STATE,
    #[error("TEE error: requested item was not found")]
    ItemNotFound = TEE_ERROR_ITEM_NOT_FOUND,
    #[error("TEE error: requested operation is not implemented in this TEE")]
    NotImplemented = TEE_ERROR_NOT_IMPLEMENTED,
    #[error("TEE error: requested operation is not supported by this TEE implementation")]
    NotSupported = TEE_ERROR_NOT_SUPPORTED,
    #[error("TEE error: no data available for the requested operation")]
    NoData = TEE_ERROR_NO_DATA,
    #[error("TEE error: system ran out of memory resources")]
    OutOfMemory = TEE_ERROR_OUT_OF_MEMORY,
    #[error("TEE error: system is busy, the requested resource is temporarily unavailable")]
    Busy = TEE_ERROR_BUSY,
    #[error("TEE error: communication failure with remote party or external system")]
    Communication = TEE_ERROR_COMMUNICATION,
    #[error("TEE error: security violation detected during operation")]
    Security = TEE_ERROR_SECURITY,
    #[error("TEE error: output buffer is too small to hold the result")]
    ShortBuffer = TEE_ERROR_SHORT_BUFFER,
    #[error("TEE error: operation was cancelled by an external event")]
    ExternalCancel = TEE_ERROR_EXTERNAL_CANCEL,
    #[error("TEE error: arithmetic overflow occurred during computation")]
    Overflow = TEE_ERROR_OVERFLOW,
    #[error("TEE error: target TA has terminated or is no longer available")]
    TargetDead = TEE_ERROR_TARGET_DEAD,
    #[error("TEE error: insufficient storage space to complete the operation")]
    StorageNoSpace = TEE_ERROR_STORAGE_NO_SPACE,
    #[error("TEE error: MAC verification failed, data integrity check failed")]
    MacInvalid = TEE_ERROR_MAC_INVALID,
    #[error("TEE error: cryptographic signature verification failed")]
    SignatureInvalid = TEE_ERROR_SIGNATURE_INVALID,
    #[error("TEE error: trusted time source has not been set or initialized")]
    TimeNotSet = TEE_ERROR_TIME_NOT_SET,
    #[error("TEE error: trusted time source requires reset or recalibration")]
    TimeNeedsReset = TEE_ERROR_TIME_NEEDS_RESET,
    #[num_enum(catch_all)]
    #[error("TEE error: unknown or unspecified error occurred")]
    Unknown(u32),
}
