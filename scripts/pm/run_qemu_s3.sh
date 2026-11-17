#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/../.." && pwd)
cpus=${S3_TEST_CPUS:-4}
rounds=${S3_TEST_ROUNDS:-12}
arch=${S3_TEST_ARCH:-x86_64}
case "$cpus" in 1|4) ;; *) echo 'S3_TEST_CPUS must be 1 or 4' >&2; exit 2;; esac
[[ "$rounds" =~ ^[1-9][0-9]?$|^100$ ]] || exit 2
output=${S3_OUTPUT_DIR:-$(mktemp -d /tmp/xkernel-s3.XXXXXX)}
mkdir -p "$output"
output=$(cd -- "$output" && pwd)
printf 'Evidence: %s\n' "$output"
trap 'printf "Evidence: %s\n" "$output"' EXIT
if [[ -e "$output/root.img" || -e "$output/root" ]]; then
  echo 'Refusing to overwrite an existing guest disk or root directory' >&2
  exit 2
fi
mkdir -p "$output/root/bin"
cd "$repo"
case "$arch" in
  x86_64) seed=scripts/pm/qemu_s3_defconfig; target=x86_64-unknown-none ;;
  aarch64) seed=platforms/kplat-aarch64/qemu_defconfig; target=aarch64-unknown-none-softfloat ;;
  riscv64) seed=platforms/kplat-riscv64/qemu_defconfig; target=riscv64gc-unknown-none-elf ;;
  *) echo 'S3_TEST_ARCH must be x86_64, aarch64 or riscv64' >&2; exit 2 ;;
esac
cp "$seed" .config
# Keep the production driver profile minimal and expose the executed PM stages.
# This never selects an alternative or mocked suspend backend.
python3 - <<'CONFIG'
from pathlib import Path
p = Path('.config')
disabled = ('KFEAT_NET', 'KFEAT_VSOCK', 'KFEAT_TEE', 'KFEAT_TEE_TA_SIGN',
            'KFEAT_TIPC', 'KFEAT_VSOCK_TIPC_BRIDGE', 'KFEAT_DRIVER_VIRTIO_GPU',
            'KFEAT_DRIVER_VIRTIO_INPUT', 'KFEAT_DRIVER_VIRTIO_RNG',
            'KFEAT_ENTROPY_VIRTIO_RNG', 'KFEAT_SNAPSHOT', 'KFEAT_WATCHDOG',
            'KFEAT_WATCHDOG_HARDLOCKUP', 'KFEAT_NMI', 'KFEAT_NMI_MODE_PSEUDO',
            'KFEAT_NMI_PMU', 'LOG_LEVEL_WARN', 'LOG_LEVEL_DEBUG', 'LOG_LEVEL_TRACE',
            'LOG_LEVEL_ERROR', 'LOG_LEVEL_INFO', 'KFEAT_ROOT_BLOCK')
lines = [line for line in p.read_text().splitlines()
         if not any(line.startswith(key + '=') or line == '# ' + key + ' is not set'
                    for key in disabled)]
lines.extend('# ' + key + ' is not set' for key in disabled
             if key not in ('LOG_LEVEL_INFO', 'KFEAT_ROOT_BLOCK'))
lines += ['LOG_LEVEL_INFO=y', 'KFEAT_ROOT_BLOCK="vda"']
p.write_text('\n'.join(lines) + '\n')
CONFIG
make defconfig
S3_TEST_CPUS="$cpus" S3_TEST_ROUNDS="$rounds" rustc --edition=2024 \
  --target "$target" -C panic=abort -C opt-level=2 \
  -C relocation-model=static -C link-arg=-e_start -C link-arg=-Ttext=0x400000 \
  scripts/pm/guest_s3.rs -o "$output/root/bin/sh"
truncate -s 256M "$output/root.img"
mkfs.ext4 -q -F -b 4096 -O '^metadata_csum,^64bit' -d "$output/root" "$output/root.img"
cp .config "$output/kernel.config"
make build > "$output/build.log" 2>&1
S3_TEST_ARCH="$arch" S3_TEST_CPUS="$cpus" S3_TEST_ROUNDS="$rounds" S3_OUTPUT_DIR="$output" \
  python3 scripts/pm/qemu_s3.py
