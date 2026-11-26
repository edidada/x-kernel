// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Second dynamic TA probe: echo 1 MiB in 4 KiB messages without storing the whole transfer.

extern crate alloc;

use std::rc::Rc;

use rust_libtipc::{
    ConnectResult, Handle, Manager, MessageResult, PortCfg, UnbufferedService, Uuid,
};

const SERVICE: &str = "com.xkernel.test.dynamic-ta-bulk";
// Stay within TIPC's 8 KiB receive-slot bound.
const CHUNK_BYTES: usize = 4 * 1024;

// 40f8e451-b549-4eeb-a907-27a52c8cd95a.
#[used]
#[unsafe(link_section = ".ta_head")]
static TA_HEAD: [u8; 32] = [
    0x51, 0xe4, 0xf8, 0x40, 0x49, 0xb5, 0xeb, 0x4e, 0xa9, 0x07, 0x27, 0xa5, 0x2c, 0x8c, 0xd9, 0x5a,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

struct BulkEchoService;

impl UnbufferedService for BulkEchoService {
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
        if count == 0 {
            return Ok(MessageResult::CloseConnection);
        }
        channel.send_vectored(&[&buffer[..count]], &[])?;
        Ok(MessageResult::MaintainConnection)
    }

    fn max_message_length(&self) -> usize {
        CHUNK_BYTES
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dynamic-ta-bulk-echo: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> rust_libtipc::Result<()> {
    let port = PortCfg::new(SERVICE)?
        .allow_ns_connect()
        .msg_queue_len(4)
        .msg_max_size(CHUNK_BYTES as u32);
    rust_libtipc::service_dispatcher! {
        pub(crate) enum BulkEchoDispatcher {
            BulkEchoService,
        }
    }
    let mut dispatcher = BulkEchoDispatcher::<1>::new()?;
    dispatcher.add_service(Rc::new(BulkEchoService), port)?;
    let manager =
        Manager::<_, Vec<u8>, 1, 8>::new_with_dispatcher(dispatcher, vec![0; CHUNK_BYTES])?;
    println!("dynamic-ta-bulk-echo: ready ({SERVICE})");
    manager.run_event_loop()
}
