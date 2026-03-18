// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rust_utee::api::tee_api_time::{TEE_GetREETime, TEE_GetSystemTime, TEE_Wait};

use rust_utee::tee_api_types::TEE_Time;

fn main() {
    let mut time = TEE_Time {
        seconds: 0,
        millis: 0,
    };
    TEE_GetSystemTime(&mut time);
    println!(
        "system time: seconds={}, millis={}",
        time.seconds, time.millis
    );
    TEE_GetREETime(&mut time);
    println!("REE time: seconds={}, millis={}", time.seconds, time.millis);
    TEE_Wait(1000);
    println!("time wait 1000");
}
