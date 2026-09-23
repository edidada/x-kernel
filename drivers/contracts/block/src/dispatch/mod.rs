// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-class dispatch services: portable waiting, deferred completion
//! reclaiming, and IRQ wiring. Model-agnostic — usable by drivers on both
//! the legacy and driver-core device models.

pub mod irq;
pub mod reclaimer;
pub mod wait;
