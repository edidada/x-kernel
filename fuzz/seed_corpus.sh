#!/bin/bash
# Install repo fixtures + generated seeds into corpus/<entrypoint>/ for wfuzz/cargo-fuzz.
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"

python3 generate_seeds.py

install_seed() {
  local target="$1"
  shift
  local dst="corpus/${target}"
  mkdir -p "$dst"
  local src base
  for src in "$@"; do
    [ -f "$src" ] || { echo "missing seed: $src" >&2; continue; }
    base="$(basename "$src")"
    cp -f "$src" "$dst/$base"
    echo "  $target <= $src"
  done
}

install_glob() {
  local target="$1"
  local pattern="$2"
  local f
  shopt -s nullglob
  for f in $pattern; do
    install_seed "$target" "$f"
  done
  shopt -u nullglob
}

echo "==> installing corpus seeds"

install_seed fdt_parse \
  "$ROOT/drivers/rs-fdtree/dtb/test.dtb" \
  "$ROOT/drivers/rs-fdtree/dtb/crosvm.dtb"

install_seed fdt_typed_helpers \
  "$ROOT/drivers/rs-fdtree/dtb/test.dtb" \
  "$ROOT/drivers/rs-fdtree/dtb/crosvm.dtb"

install_seed user_elf_parse \
  "$ROOT/boot/kernel_elf_parser/tests/elf_static" \
  "$ROOT/boot/kernel_elf_parser/tests/ld-linux-x86-64.so.2"

install_seed kernel_elf_parse \
  "$ROOT/boot/kernel_elf_parser/tests/elf_static" \
  "$ROOT/boot/kernel_elf_parser/tests/ld-linux-x86-64.so.2"

install_seed kbpf_vm_run \
  seeds/kbpf/*.bin

install_seed rsext4_sb_decode \
  seeds/rsext4/ext4_superblock.bin \
  seeds/rsext4/ext4_superblock_extents.bin \
  seeds/rsext4/ext4_superblock_journal.bin \
  seeds/rsext4/ext4_superblock_64bit_flex.bin

install_seed rsext4_gdt_walk \
  seeds/rsext4/ext4_gdt_32.bin \
  seeds/rsext4/ext4_gdt_64.bin \
  seeds/rsext4/ext4_gdt_32_16.bin \
  seeds/rsext4/ext4_gdt_32_raw.bin \
  seeds/rsext4/ext4_gdt_64_raw.bin

install_seed rsext4_dirent_parse \
  seeds/rsext4/ext4_dirent_chain.bin \
  seeds/rsext4/ext4_dirent_longname.bin
install_seed rsext4_inode_decode \
  seeds/rsext4/ext4_inode_128.bin \
  seeds/rsext4/ext4_inode_256.bin \
  seeds/rsext4/ext4_inode_dir_128.bin \
  seeds/rsext4/ext4_inode_symlink_256.bin

install_seed rsext4_extent_parse \
  seeds/rsext4/ext4_extent_leaf.bin \
  seeds/rsext4/ext4_extent_index.bin

install_seed firmware_handoff_memmap \
  seeds/firmware_handoff/efi_memmap.bin \
  seeds/firmware_handoff/efi_memmap_mixed.bin

install_seed tee_sm2_sec1 \
  seeds/tee/sm2_generator.sec1 \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/int-p256.der" \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/leaf-p256-110d-post-sc081-100d.der"

install_glob tee_sm2_sec1 \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/leaf-p256*.der"

install_glob tee_rsa_der \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/leaf-rsa*.der"

install_seed tee_rsa_der \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/int-rsa2048.der" \
  "$ROOT/tee/tee_crypto/tests/fixtures/pkix/policy-checks/codesign-self-signed-365d.der"

if [ "${1:-}" = "--wfuzz-input" ]; then
  echo "==> syncing into wfuzz-test-*/1/input"
  for target_dir in wfuzz-test-*/1/input; do
    [ -d "$target_dir" ] || continue
    cp -f corpus/*/* "$target_dir/" 2>/dev/null || true
  done
fi

echo ""
echo "done. corpus layout:"
find corpus -type f | sort
