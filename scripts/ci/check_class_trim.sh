#!/usr/bin/env bash
# kclass class-feature trimming check.
#
# Rationale: five separate regressions in this PR series came from "changed
# cfg-gated code, verified only the currently-built feature combinations"
# (activate_mmio, ns16550, ramdisk, CHAR_CONSOLE_HOOK). The driver matrix
# covers DRIVER combinations, but kclass's design premise — each class
# feature independently selectable — needs builds WITHOUT the entangled
# defaults (char is always on via the console path in every standard
# defconfig, which is exactly how CHAR_CONSOLE_HOOK survived CI green).
#
# This check builds one entry-level configuration with the char class off
# (early console backend kept: it is a platform requirement, decoupled from
# KFEAT_CHAR) and every other class driver present. Run from `make clippy`
# so every commit and CI run exercises it.
set -euo pipefail
cd "$(dirname "$0")/../.."

TARGET=aarch64-unknown-none-softfloat
CFG_BAK="$(mktemp)"
XCONF_BAK="$(mktemp)"
trap 'cp "$CFG_BAK" .config 2>/dev/null || true; cp "$XCONF_BAK" .cargo/.xconfig.toml 2>/dev/null || true; rm -f "$CFG_BAK" "$XCONF_BAK"' EXIT
cp .config "$CFG_BAK"
cp .cargo/.xconfig.toml "$XCONF_BAK"

cp platforms/kplat-aarch64/qemu_defconfig .config
{
    echo '# KFEAT_CHAR is not set'
    echo '# KFEAT_DRIVER_SERIAL_PL011 is not set'
    echo 'KFEAT_DRIVER_CONSOLE_PL011=y'
} >> .config
make olddefconfig >/dev/null 2>&1
# Regenerate the platform build state for THIS configuration: kbuild_config
# copies the last generated config.rs, kplat crates compile empty unless the
# k_plat_name rustflags cfg matches, and .cargo/.xconfig.toml carries both
# from whatever xkmake ran last. Both are regenerated here and restored on
# exit.
make gen-const >/dev/null 2>&1
make -s gen-cargo >/dev/null 2>&1
# Loud verification instead of a silent `|| true`: a wrong-platform or
# symbol-less config.rs here would surface as a confusing downstream
# compile error.
gen=target/kbuild/kplat-aarch64/config.rs
grep -q 'pub const ARCH: &str = "aarch64"' "$gen" \
    || { echo "!!! class-trim: gen-const produced a non-aarch64 config" >&2; exit 1; }
grep -q ARM_GENERIC_TIMER_MODE "$gen" \
    || { echo "!!! class-trim: gen-const config lacks ARM_GENERIC_TIMER_MODE" >&2; exit 1; }

feats="$(python3 scripts/kfeat-features.py)"
if echo "$feats" | tr ',' '\n' | grep -qE '^kfeat/char$'; then
    echo "!!! class-trim: KFEAT_CHAR could not be disabled (reverse dependency?)" >&2
    exit 1
fi

# kbuild_config copies the generated config in its build script; cached
# copies (host path AND per-target path) from earlier configurations must
# not survive into this check.
rm -rf target/class-trim/debug/build/kbuild_config-*
rm -rf target/class-trim/*/debug/build/kbuild_config-*

echo ">>> [class-trim: no char class] cargo check"
if ! K_PLAT_NAME=kplat-aarch64 RUST_TARGET_PATH=platforms/kplat-aarch64 \
    cargo check --manifest-path entry/Cargo.toml --target "$TARGET" \
    --target-dir target/class-trim -p entry --features "$feats" --quiet; then
    echo "!!! [class-trim] FAILED — a class feature does not compile without char" >&2
    exit 1
fi
echo "class-trim: char-less build compiles"
