// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dynamic TA probe image uploaded by the host CA; it is never installed in the guest image.

extern crate alloc;

use std::rc::Rc;

use rust_libtipc::{
    ConnectResult, Handle, Manager, MessageResult, PortCfg, UnbufferedService, Uuid,
};

const SERVICE: &str = "com.xkernel.test.dynamic-ta";

// 8ea3a3d8-c164-4f58-98a2-b4c06d1ba4e7.
#[used]
#[unsafe(link_section = ".ta_head")]
static TA_HEAD: [u8; 32] = [
    0xd8, 0xa3, 0xa3, 0x8e, 0x64, 0xc1, 0x58, 0x4f, 0x98, 0xa2, 0xb4, 0xc0, 0x6d, 0x1b, 0xa4, 0xe7,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

struct ProbeService;

impl UnbufferedService for ProbeService {
    type Connection = ();

    fn on_connect(
        &self,
        _port: &PortCfg,
        _channel: &Handle,
        _peer: &Uuid,
    ) -> rust_libtipc::Result<ConnectResult<Self::Connection>> {
        Ok(ConnectResult::Accept(()))
    }

    fn on_message(
        &self,
        _connection: &Self::Connection,
        channel: &Handle,
        buffer: &mut [u8],
    ) -> rust_libtipc::Result<MessageResult> {
        let count = channel.recv_into(buffer)?;
        if &buffer[..count] != b"ping" {
            return Ok(MessageResult::CloseConnection);
        }
        channel.send_vectored(&[b"pong"], &[])?;
        Ok(MessageResult::MaintainConnection)
    }

    fn max_message_length(&self) -> usize {
        4
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dynamic-ta-pingpong: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> rust_libtipc::Result<()> {
    let port = PortCfg::new(SERVICE)?
        .allow_ns_connect()
        .msg_queue_len(2)
        .msg_max_size(4);
    rust_libtipc::service_dispatcher! {
        pub(crate) enum ProbeDispatcher {
            ProbeService,
        }
    }
    let mut dispatcher = ProbeDispatcher::<1>::new()?;
    dispatcher.add_service(Rc::new(ProbeService), port)?;
    let manager = Manager::<_, Vec<u8>, 1, 8>::new_with_dispatcher(dispatcher, vec![0; 4])?;
    println!("dynamic-ta-pingpong: ready ({SERVICE})");
    manager.run_event_loop()
}
