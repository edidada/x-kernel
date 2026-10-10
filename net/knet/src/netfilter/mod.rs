// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal network-filter policy representation.

#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: packet evaluation and legacy iptables consumers \
              follow in IKIXDM"
)]
mod filter;
