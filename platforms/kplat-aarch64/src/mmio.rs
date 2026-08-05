// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type-1 host MMIO preparation hooks.

pub(crate) fn prepare_boot_memory() {}

kplat::default_mmio_if_impl!();
