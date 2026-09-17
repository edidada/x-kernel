// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal IPv4 netfilter representation.
//!
//! Legacy iptables ABI data must be decoded and validated before it enters
//! this module. The model stores owned semantic values and does not retain
//! offsets or pointers into the userspace replacement blob.

#[expect(
    dead_code,
    reason = "the legacy ABI decoder and packet evaluator are implemented in later stages"
)]
pub(crate) mod model;
