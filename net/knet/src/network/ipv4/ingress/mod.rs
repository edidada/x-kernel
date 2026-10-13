// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 input admission, fragment expiration, and transport delivery.

mod admission;
mod batch;
mod delivery;
mod processor;
mod timer;

pub(crate) use batch::IngressBatch;
pub(crate) use processor::Ipv4Ingress;
