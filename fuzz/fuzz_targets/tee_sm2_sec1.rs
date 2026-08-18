// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tee_crypto::sm2::sm2_validate_sec1_public_key(data);
    if data.len() >= 65 {
        let _ = tee_crypto::sm2::sm2_validate_sec1_public_key(&data[..65]);
    }
    let _ = tee_crypto::sm2::sm2_secret_scalar_from_pkcs8_der(data);
    if data.len() >= 64 {
        let mut pk = [0u8; 64];
        pk.copy_from_slice(&data[..64]);
        let _ = tee_crypto::sm2::sm2_compute_sign_digest(None, data, &pk);
    }
});
