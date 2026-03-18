// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_char;
use rust_utee::api::tee_api_property::*;
use rust_utee::tee_api_defines::*;

fn main() {
    let mut buf = vec![0u8; 128];
    let mut len: usize = buf.len();
    let name = "gpd.client.identity\0".as_ptr() as *const c_char;
    let res = TEE_GetPropertyAsString(
        TEE_PROPSET_CURRENT_CLIENT,
        name,
        buf.as_mut_ptr() as _,
        &mut len,
    );
    let s = String::from_utf8_lossy(&buf[..len]);
    println!("TEE_GetPropertyAsString: {}", s);
    println!("TEE_GetPropertyAsString: {:x}", res);

    let mut test_value = false;
    let bool_name = "gpd.ta.multiSession\0".as_ptr() as *const c_char;
    let res = TEE_GetPropertyAsBool(TEE_PROPSET_CURRENT_TA, bool_name, &mut test_value);
    println!("TEE_GetPropertyAsBool: {:x}", res);
    println!("TEE_GetPropertyAsBool: {}", test_value);
}
