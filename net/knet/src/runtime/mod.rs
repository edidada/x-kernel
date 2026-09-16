// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Poll-round runtime for knet.
//!
//! This module owns Service assembly, background/assist polling, reusable packet
//! batches, and the NetIo queue mutex domain. It is internal to the knet crate and
//! is not a cross-crate shared library surface.

pub(crate) mod dataplane;
pub(crate) mod io;
pub(crate) mod poll_round;
pub(crate) mod poller;
pub(crate) mod service;
