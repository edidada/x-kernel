// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Management ABI adapters for the shared semantic filter configuration API.
//!
//! Adapters own wire decoding, encoding, and protocol-specific metadata. They
//! construct validated semantic tables and use `filter::config` for snapshots,
//! conditional publication, and counter additions.

pub(super) mod iptables_legacy;
