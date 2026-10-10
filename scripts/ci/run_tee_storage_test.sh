#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: run_tee_storage_test.sh <arch>}"
musl_target="${arch}-unknown-linux-musl"
musl_linker="${arch}-linux-musl-gcc"
target_upper="${musl_target^^}"
target_upper="${target_upper//-/_}"
read -r -a tee_test_bins <<< "${TEE_TEST_BINS:-storage_test cryp_test}"

: "${AUX_RUST_TOOLCHAIN:?AUX_RUST_TOOLCHAIN is required}"
: "${LIBUTEE_REPO:?LIBUTEE_REPO is required}"
: "${TARGET_DIR:?TARGET_DIR is required}"
: "${HOSTFWD_PORT:?HOSTFWD_PORT is required}"
: "${VSOCK_CID:?VSOCK_CID is required}"

if [ "${#tee_test_bins[@]}" -eq 0 ]; then
    echo "TEE_TEST_BINS must contain at least one test binary" >&2
    exit 2
fi

libutee_dir="/xkernel-target/libutee-${arch}"
mkdir -p "$(dirname "${libutee_dir}")"
echo "==> Syncing rust-libutee..."
fallback_clone_dir=""
cleanup_fallback_clone() {
    if [ -n "${fallback_clone_dir}" ]; then
        rm -rf "${fallback_clone_dir}"
    fi
}
trap cleanup_fallback_clone EXIT
for attempt in 1 2 3; do
    if [ -d "${libutee_dir}/.git" ]; then
        if git -C "${libutee_dir}" fetch --depth 1 origin HEAD &&
            git -C "${libutee_dir}" reset --hard FETCH_HEAD; then
            break
        fi
    fi
    # A stale or damaged cache must not force every retry through the same
    # repository. Clone into a task-owned directory without deleting the cache;
    # another job may still use it, and it remains available for diagnosis.
    fallback_clone_dir=$(mktemp -d "${libutee_dir}.clone.XXXXXX")
    if git clone --depth 1 "${LIBUTEE_REPO}" "${fallback_clone_dir}"; then
        # Publish a completed cold cache without replacing a concurrent job's
        # directory. Damaged or already occupied caches keep the private copy.
        if [ ! -e "${libutee_dir}" ] &&
            mv -T -n "${fallback_clone_dir}" "${libutee_dir}" &&
            [ ! -d "${fallback_clone_dir}" ]; then
            fallback_clone_dir=""
        else
            libutee_dir="${fallback_clone_dir}"
        fi
        break
    fi
    cleanup_fallback_clone
    fallback_clone_dir=""
    if [ "${attempt}" -eq 3 ]; then
        echo "rust-libutee sync failed after ${attempt} attempts" >&2
        exit 1
    fi
    sleep "$((attempt * 5))"
done

echo "==> Building TEE tests for ${musl_target}: ${tee_test_bins[*]}"
(
    cd "${libutee_dir}"
    for bin in "${tee_test_bins[@]}"; do
        CC="${musl_linker}" cargo +"${AUX_RUST_TOOLCHAIN}" build \
            --bin "${bin}" --release --target "${musl_target}"
    done
)

tee_init_apps=""
for bin in "${tee_test_bins[@]}"; do
    app_path="/tee/${bin}"
    if [ -n "${tee_init_apps}" ]; then
        tee_init_apps+=","
    fi
    tee_init_apps+="${app_path}"
done

echo "==> Building tee_apps/sh with TEE_INIT_APPS=${tee_init_apps}..."
env \
  TEE_INIT_APPS="${tee_init_apps}" \
  RUSTFLAGS= \
  CC="${musl_linker}" \
  "CARGO_TARGET_${target_upper}_LINKER=${musl_linker}" \
  cargo build --release --target "${musl_target}" --manifest-path tee_apps/sh/Cargo.toml \
  --target-dir "${TARGET_DIR}/tee-apps"

echo "==> Creating rootfs..."
staging_dir="${TARGET_DIR}/rootfs-staging"
rm -rf "${staging_dir}"
mkdir -p "${staging_dir}/bin" "${staging_dir}/tee"
cp "${TARGET_DIR}/tee-apps/${musl_target}/release/sh" "${staging_dir}/bin/sh"
chmod 0755 "${staging_dir}/bin/sh"
for bin in "${tee_test_bins[@]}"; do
    cp "${libutee_dir}/target/${musl_target}/release/${bin}" "${staging_dir}/tee/${bin}"
    chmod 0755 "${staging_dir}/tee/${bin}"
done
truncate -s 64M disk.img
mkfs.ext4 -q -F -b 4096 -O ^metadata_csum,^64bit -d "${staging_dir}" disk.img

if [ "${SKIP_KERNEL_BUILD:-0}" != "1" ]; then
    echo "==> Building kernel..."
    cp "platforms/kplat-${arch}/qemu_defconfig" .config
    make build
else
    echo "==> Reusing prebuilt kernel artifact..."
fi

echo "==> Running TEE tests..."
printf 'TEE_TEST_APPS: %s\n' "${tee_test_bins[*]}" > tee-test-output.log
set +e
timeout 1200 stdbuf -oL -eL make HOSTFWD_PORT="${HOSTFWD_PORT}" VSOCK_CID="${VSOCK_CID}" justrun 2>&1 | tee -a tee-test-output.log
qemu_status=${PIPESTATUS[0]}
set -e

if [ "${qemu_status}" -eq 124 ]; then
    echo "TEE_RESULT: TIMEOUT" | tee -a tee-test-output.log
elif [ "${qemu_status}" -ne 0 ]; then
    echo "TEE_RESULT: QEMU_ERROR(${qemu_status})" | tee -a tee-test-output.log
fi
