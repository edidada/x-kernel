// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tee_crypto::tee_ops::rsa::rsa_public_key_from_pkcs1_der(data);
    let _ = tee_crypto::tee_ops::rsa::rsa_public_key_from_spki_der(data);
    let _ = tee_crypto::tee_ops::rsa::rsa_key_components_from_pkcs1_der(data);
    let _ = tee_crypto::tee_ops::rsa::rsa_private_key_from_pkcs8_der(data);
    let _ = tee_crypto::tee_ops::rsa::rsa_private_key_from_pkcs1_der(data);

    for end in (64..=data.len()).step_by(64).take(8) {
        let slice = &data[..end];
        let _ = tee_crypto::tee_ops::rsa::rsa_public_key_from_pkcs1_der(slice);
        let _ = tee_crypto::tee_ops::rsa::rsa_public_key_from_spki_der(slice);
        let _ = tee_crypto::tee_ops::rsa::rsa_key_components_from_pkcs1_der(slice);
    }
});
