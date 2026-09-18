// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table model, matcher, and evaluator.

#[cfg_attr(
    not(unittest),
    expect(
        dead_code,
        reason = "filter hook integration is implemented in a later stage"
    )
)]
mod evaluator;
#[cfg_attr(
    not(unittest),
    expect(
        dead_code,
        reason = "filter hook integration is implemented in a later stage"
    )
)]
mod matcher;
#[expect(
    dead_code,
    reason = "the legacy ABI decoder and hook integration are implemented in later stages"
)]
mod model;
