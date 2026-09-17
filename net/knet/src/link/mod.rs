// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Link-layer socket support.

use kerrno::{KError, KResult};

use crate::{netns, service::Service};

pub(crate) mod buf;
pub mod packet;
pub(crate) mod wire;

/// Sends a complete link-layer frame through `net_ns`'s interface `ifindex`.
pub(crate) fn send_link_frame_in(net_ns: &Service, ifindex: i32, frame: &[u8]) -> KResult<usize> {
    net_ns.send_link_frame(ifindex, frame)
}

/// Sends a complete link-layer frame through the interface identified by `ifindex`.
pub fn send_link_frame(ifindex: i32, frame: &[u8]) -> KResult<usize> {
    let Some(init_net) = netns::init_net() else {
        return Err(KError::OperationNotSupported);
    };
    send_link_frame_in(init_net.service(), ifindex, frame)
}
