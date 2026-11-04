// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device descriptors, objects, handles, and resource metadata.

pub mod desc;
pub mod handles;
/// Device-owned cleanup of resources acquired through explicit OS providers.
pub mod managed;
pub mod object;
pub mod resource;
