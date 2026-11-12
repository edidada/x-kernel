#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/../.." && pwd)
cpus=${S3_TEST_CPUS:-4}
rounds=${S3_TEST_ROUNDS:-12}
case "$cpus" in 1|4) ;; *) echo 'S3_TEST_CPUS must be 1 or 4' >&2; exit 2;; esac
[[ "$rounds" =~ ^[1-9][0-9]*$ ]] || exit 2
output=${S3_OUTPUT_DIR:-$(mktemp -d /tmp/xkernel-s3.XXXXXX)}
mkdir -p "$output/root/bin"
output=$(cd -- "$output" && pwd)
printf 'Evidence: %s\n' "$output"
trap 'printf "Evidence: %s\n" "$output"' EXIT
cd "$repo"
cp scripts/pm/qemu_s3_defconfig .config
make defconfig
S3_TEST_CPUS="$cpus" S3_TEST_ROUNDS="$rounds" rustc --edition=2024 \
  --target x86_64-unknown-none -C panic=abort -C opt-level=2 \
  -C relocation-model=static -C link-arg=-e_start -C link-arg=-Ttext=0x400000 \
  scripts/pm/guest_s3.rs -o "$output/root/bin/sh"
truncate -s 256M "$output/root.img"
mkfs.ext4 -q -F -b 4096 -O '^metadata_csum,^64bit' -d "$output/root" "$output/root.img"
make build > "$output/build.log" 2>&1
S3_TEST_CPUS="$cpus" S3_TEST_ROUNDS="$rounds" S3_OUTPUT_DIR="$output" \
  python3 scripts/pm/qemu_s3.py
