// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Compile-time environment for the real kcpu_id_map dependency. The harnesses
// use only LogicalCpuId, not firmware discovery or the runtime CPU map.
pub const NR_CPUS: usize = 4;
