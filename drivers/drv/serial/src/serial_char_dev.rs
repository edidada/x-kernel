// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `/dev` view of one UART: the char-class adapter every serial
//! driver publishes. One wrapper, not one copy per driver.

use alloc::sync::Arc;

use kerrno::KResult;
use serial_core::SerialPort;
use subsys_char::CharDevice;

pub(crate) struct SerialCharDev(pub(crate) Arc<SerialPort>);

impl CharDevice for SerialCharDev {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        Ok(self.0.read_data(buf))
    }

    fn write(&self, buf: &[u8]) -> KResult<usize> {
        self.0.write_data(buf);
        Ok(buf.len())
    }
}
