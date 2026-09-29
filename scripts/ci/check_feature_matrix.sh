#!/usr/bin/env bash
# Feature-combination compile matrix for the driver framework.
#
# Rationale: legal-but-never-built feature combinations have shipped real
# compile errors twice (five activate_mmio paths, ns16550 ioport) — see
# docs/driver-framework-review.md §8.2/§8.4. This script is the local,
# repeatable form of the manual verification done in those fixes; wire it
# into CI (Jenkins stage) or run before merges touching drivers/.
#
# Usage: scripts/ci/check_feature_matrix.sh [target]   (default aarch64)
set -euo pipefail
cd "$(dirname "$0")/../.."

TARGET="${1:-aarch64-unknown-none-softfloat}"
BASE="$(python3 scripts/kfeat-features.py)"
X86_BASE=""   # filled below for the x86 pass

# Combinations that previously shipped broken (each entry: label features...)
MATRIX=(
    "mmio+rng     kfeat/driver_virtio_mmio,kfeat/driver_virtio_rng"
    "mmio+gpu     kfeat/driver_virtio_mmio,kfeat/driver_virtio_gpu"
    "mmio+input   kfeat/driver_virtio_mmio,kfeat/driver_virtio_input"
    "mmio+vsock   kfeat/driver_virtio_mmio,kfeat/driver_virtio_socket"
    "mmio+9p      kfeat/driver_virtio_mmio,kfeat/driver_virtio_9p"
)

fail=0
for entry in "${MATRIX[@]}"; do
    label="${entry%% *}"; feats="${entry#* }"
    echo ">>> [$label] cargo check (+$feats)"
    if ! cargo check --manifest-path entry/Cargo.toml --target "$TARGET" \
        --target-dir target -p entry --features "$BASE,$feats" --quiet; then
        echo "!!! [$label] FAILED"
        fail=1
    fi
done

# The ioport serial variant only builds on x86_64.
echo ">>> [ns16550-ioport @x86_64] cargo check"
cp .config /tmp/feat-matrix-config.bak
cp platforms/kplat-x86_64/qemu_defconfig .config
printf 'KFEAT_ALLOC=y\nKFEAT_ALLOC_SLAB=y\nKFEAT_DRIVER_VIRTIO_BLK=y\nKFEAT_DRIVER_VIRTIO_NET=y\nKFEAT_DRIVER_VIRTIO_GPU=y\nKFEAT_DRIVER_VIRTIO_INPUT=y\n' >> .config
X86_BASE="$(python3 scripts/kfeat-features.py)"
cp /tmp/feat-matrix-config.bak .config
if ! cargo check --manifest-path entry/Cargo.toml --target x86_64-unknown-none \
    --target-dir target -p entry --features "$X86_BASE" --quiet; then
    echo "!!! [ns16550-ioport @x86_64] FAILED"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo "feature matrix: FAILURES above"
    exit 1
fi
echo "feature matrix: all combinations compile"
