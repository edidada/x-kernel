#!/usr/bin/env bash
# Feature-combination compile matrix for the driver framework.
#
# Rationale: legal-but-never-built feature combinations have shipped real
# compile errors FOUR times (five activate_mmio paths, ns16550 ioport,
# ramdisk PlatformIdentity) — see docs/driver-framework-review.md §8.2/§8.4.
# The matrix is DERIVED from drivers/Kconfig `KFEAT_DRIVER_*` symbols, not
# hand-enumerated: every new driver symbol automatically gains a matrix row,
# so coverage cannot lag behind driver count.
#
# Strategy per symbol S: compile base+S, and — for the virtio families —
# base+mmio+S (the combination class that broke twice).
#
# Usage: scripts/ci/check_feature_matrix.sh [target]   (default aarch64)
set -euo pipefail
cd "$(dirname "$0")/../.."

TARGET="${1:-aarch64-unknown-none-softfloat}"

# Kconfig duplicate-symbol guard: rebase resolutions have twice left two
# definitions of one KFEAT symbol (different depends/defaults silently
# merging). A duplicate makes every derived feature list ambiguous.
dups="$(grep -oE '^config [A-Z0-9_]+' drivers/Kconfig | sort | uniq -d)"
if [ -n "$dups" ]; then
    echo "!!! duplicate Kconfig symbols:" >&2
    echo "$dups" >&2
    exit 1
fi

BASE="$(python3 scripts/kfeat-features.py)"

# KFEAT_DRIVER_* → kfeat feature name (lowercase, dash→underscore like
# scripts/kfeat-features.py does for all symbols).
mapfile -t SYMBOLS < <(grep -oE 'config KFEAT_DRIVER_[A-Z0-9_]+' drivers/Kconfig \
    | awk '{print tolower($2)}' | sed 's/^kfeat_//')

# Arch-bound symbols are derived from Kconfig `depends on ... ARCH_X86_64`
# (hand-maintaining this list is how coverage rots).
declare -A X86_ONLY=()
while IFS=' ' read -r sym; do
    X86_ONLY["$sym"]=1
done < <(python3 - <<'PYKCONFIG'
import re
text = open("drivers/Kconfig").read()
# config 块：从 "config SYM" 到下一个 "config "/"comment "/"menu" 顶格行
PAT = "^config (KFEAT_DRIVER_[A-Z0-9_]+)((?:\\n(?!config |comment |menu |choice ).*)*)"
for m in re.finditer(PAT, text, re.M):
    sym, body = m.group(1), m.group(2)
    deps = re.search(r"^\s*depends on (.+)$", body, re.M)
    if deps and "ARCH_X86_64" in deps.group(1):
        print(sym[6:].lower())
PYKCONFIG
)

declare -A MATRIX=()   # label -> features (ordered, deduped)
add() { MATRIX["$1"]+=",kfeat/${2//,/,kfeat/}"; }

for sym in "${SYMBOLS[@]}"; do
    if [[ -n "${X86_ONLY[$sym]:-}" ]]; then continue; fi
    add "$sym" "$sym"
    # mmio transport × each virtio family: the historically broken class.
    if [[ "$sym" == driver_virtio_* && "$sym" != driver_virtio_mmio ]]; then
        add "mmio+${sym#driver_virtio_}" "driver_virtio_mmio,$sym"
    fi
done

# KFEAT_DRIVER_RAMDISK_STATIC embeds an image at compile time
# (include_bytes!(env!("XKERNEL_RAMDISK_IMG"))); the full build flow provides
# one. Type-check it standalone with a small stub image.
STUB_IMG="$(mktemp)"
trap 'rm -f "$STUB_IMG"' EXIT
dd if=/dev/zero of="$STUB_IMG" bs=4096 count=1 status=none
export XKERNEL_RAMDISK_IMG="$STUB_IMG"
export XKERNEL_RAMDISK_IMG_LEN=4096

fail=0
for label in "${!MATRIX[@]}"; do
    feats="${MATRIX[$label],}"   # strip leading comma
    echo ">>> [$label] cargo check (+$feats)"
    if ! cargo check --manifest-path entry/Cargo.toml --target "$TARGET" \
        --target-dir target -p entry --features "$BASE$feats" --quiet; then
        echo "!!! [$label] FAILED"
        fail=1
    fi
done

# --- kclass per-class trim: each class driver ALONE (char class off) ---
# The combined class-trim runs from `make clippy`; here every class gets a
# build with ONLY its own driver enabled, verifying the "independently
# selectable" premise per class.
declare -A CLASS_DRIVER=(
    [block]=KFEAT_DRIVER_VIRTIO_BLK
    [net]=KFEAT_DRIVER_VIRTIO_NET
    [display]=KFEAT_DRIVER_VIRTIO_GPU
    [input]=KFEAT_DRIVER_VIRTIO_INPUT
    [hwrng]=KFEAT_DRIVER_VIRTIO_RNG
    [vsock]=KFEAT_DRIVER_VIRTIO_SOCKET
    [virtio-9p]=KFEAT_DRIVER_VIRTIO_9P
)
CFG_BAK="$(mktemp)"
XCONF_BAK="$(mktemp)"
XCONF_HAD=0
[ -f .cargo/.xconfig.toml ] && XCONF_HAD=1 && cp .cargo/.xconfig.toml "$XCONF_BAK"
# Restore the caller's build state on ANY exit: a mid-loop abort under
# `set -e` used to leave the trim configuration active, making every
# later bare make/cargo silently build the wrong thing.
trap 'cp "$CFG_BAK" .config 2>/dev/null || true; if [ "$XCONF_HAD" = 1 ]; then cp "$XCONF_BAK" .cargo/.xconfig.toml 2>/dev/null || true; else rm -f .cargo/.xconfig.toml; fi; rm -f "$CFG_BAK" "$XCONF_BAK"' EXIT
cp .config "$CFG_BAK"
for cls in block net display input hwrng vsock virtio-9p; do
    cp platforms/kplat-aarch64/qemu_defconfig .config
    {
        echo '# KFEAT_CHAR is not set'
        echo '# KFEAT_DRIVER_SERIAL_PL011 is not set'
        echo 'KFEAT_DRIVER_CONSOLE_PL011=y'
        for d in "${CLASS_DRIVER[@]}"; do echo "# $d is not set"; done
        echo "${CLASS_DRIVER[$cls]}=y"
        [ "$cls" = "virtio-9p" ] && echo 'KFEAT_FS_9P=y'
    } >> .config
    make olddefconfig >/dev/null 2>&1
    # Regenerate the aarch64 build state (kbuild config.rs + k_plat_name
    # rustflags cfg): a stale one from another platform breaks bare cargo.
    make gen-const >/dev/null 2>&1
    make -s gen-cargo >/dev/null 2>&1
    trim_feats="$(python3 scripts/kfeat-features.py)"
    echo ">>> [trim:$cls] cargo check (char off, only $cls driver)"
    rm -rf target/class-trim/debug/build/kbuild_config-*
    rm -rf target/class-trim/*/debug/build/kbuild_config-*
    if ! K_PLAT_NAME=kplat-aarch64 RUST_TARGET_PATH=platforms/kplat-aarch64 \
        cargo check --manifest-path entry/Cargo.toml --target "$TARGET" \
        --target-dir target/class-trim -p entry --features "$trim_feats" --quiet; then
        echo "!!! [trim:$cls] FAILED"
        fail=1
    fi
done
cp "$CFG_BAK" .config
cp "$XCONF_BAK" .cargo/.xconfig.toml
rm -f "$CFG_BAK" "$XCONF_BAK"
# Restore the build state for the restored .config too, so anything after
# this script (or the user's next bare cargo) sees consistent files.
make gen-const >/dev/null 2>&1
make -s gen-cargo >/dev/null 2>&1

# x86 pass: the base driver set that must build there (covers the ioport
# serial/console variants).
cp .config /tmp/feat-matrix-config.bak
cp platforms/kplat-x86_64/qemu_defconfig .config
printf 'KFEAT_ALLOC=y\nKFEAT_ALLOC_SLAB=y\nKFEAT_DRIVER_VIRTIO_BLK=y\nKFEAT_DRIVER_VIRTIO_NET=y\nKFEAT_DRIVER_VIRTIO_GPU=y\nKFEAT_DRIVER_VIRTIO_INPUT=y\n' >> .config
X86_BASE="$(python3 scripts/kfeat-features.py)"
cp /tmp/feat-matrix-config.bak .config
echo ">>> [x86_64 base+drivers] cargo check"
if ! cargo check --manifest-path entry/Cargo.toml --target x86_64-unknown-none \
    --target-dir target -p entry --features "$X86_BASE" --quiet; then
    echo "!!! [x86_64 base+drivers] FAILED"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo "feature matrix: FAILURES above"
    exit 1
fi
echo "feature matrix: ${#MATRIX[@]} aarch64 combinations + x86_64 pass all compile"
