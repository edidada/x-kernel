#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

set -eu

target=aarch64-unknown-linux-musl
ta_uuid=081ba88f-f1ee-452e-b5e8-a7e9ef173a97
release_dir="target/$target/release"
signing_dir=target/signing

cargo build --release --target "$target"
mkdir -p "$signing_dir"
if [ ! -f "$signing_dir/key.pem" ] || [ ! -f "$signing_dir/cert.der" ]; then
    openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
        -subj /CN=X-Kernel-QEMU-Apploader-Test \
        -keyout "$signing_dir/key.pem" -out "$signing_dir/cert.pem"
    openssl x509 -in "$signing_dir/cert.pem" -outform DER \
        -out "$signing_dir/cert.der"
fi

tasign-tool sign --elf "$release_dir/apploader" \
    --out-dir "$signing_dir" --algorithm rsa-sha256 \
    --leaf-cert "$signing_dir/cert.der" \
    --leaf-key "$signing_dir/key.pem" --leaf-key-pass ""
tasign-tool write-elf \
    --input-elf "$release_dir/apploader" \
    --signature-bin "$signing_dir/signature.bin" \
    --output-elf "$release_dir/$ta_uuid.ta" \
    --objcopy rust-objcopy
tasign-tool verify-elf --elf "$release_dir/$ta_uuid.ta"
