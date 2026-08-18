# x-kernel WFUZZ harness

Fuzz targets for selected `#![no_std]` (or host-friendly) crates. Each target takes
random bytes and exercises parse / validate paths without bringing up the full kernel.

## Layout

```
fuzz/
├── wfuzz.json                 # WFUZZ project config
├── fuzz_build.sh              # local build helper (nightly toolchain)
├── extract_entrypoints_list.py
├── Cargo.toml
├── fuzz_targets/
│   ├── fdt_parse.rs
│   ├── fdt_typed_helpers.rs
│   ├── kernel_elf_parse.rs
│   ├── user_elf_parse.rs
│   ├── kbpf_vm_run.rs
│   ├── rsext4_*.rs
│   ├── firmware_handoff_memmap.rs
│   ├── fs9p_*.rs
│   ├── kext4_*.rs
│   └── tee_*.rs
└── .cargo/config.toml         # Kylin mirror (container)
```

Build artifacts (`entrypoints.json`, `entrypoints_list.txt`, `wfuzz-fuzz-targets/`,
`wfuzz-test-*/`, `Cargo.lock`, `target/`, `corpus/`, `seeds/`) are generated under
`fuzz/` when running `./fuzz_build.sh` / `cargo` and are listed in the repo
`.gitignore` where applicable.

## Corpus seeds

Before platform runs (or after harness changes), install fixture seeds:

```bash
cd /code/fuzz
./seed_corpus.sh
```

This copies DTB/ELF/DER fixtures from the tree and generates ext4/EFI/kbpf seeds via
`generate_seeds.py` into `corpus/<entrypoint>/`. Platform `wfuzz fuzz` picks them
up from the test input directory on the next run.

## Container workflow (v11-2603)

```bash
docker run -it --name fuzz-xkernel -v /path/to/x-kernel:/code v11-2603:latest bash

# once as root
chown -R ubuntu:ubuntu /code

su ubuntu
cd /code/fuzz
source ~/.bashrc   # wfuzz in PATH after SDK install

./fuzz_build.sh

WFUZZ_TEST_ENTRYPOINT=fdt_parse wfuzz fuzz
# or: WFUZZ_TEST_ENTRYPOINT=kernel_elf_parse wfuzz fuzz
```

`fuzz_build.sh` uses `RUSTUP_TOOLCHAIN=nightly` because the Kylin mirror may not ship the pinned `rust-toolchain.toml` version. The crates.io mirror is configured only in `fuzz/.cargo/config.toml` and does not affect root `make build`.

## Entrypoints

| Name | Harness | Crate under test | Notes |
|------|---------|------------------|-------|
| `fdt_parse` | `fuzz_targets/fdt_parse.rs` | `rs_fdtree` | `LinuxFdt::new` + node/property walk |
| `fdt_typed_helpers` | `fuzz_targets/fdt_typed_helpers.rs` | `rs_fdtree` | chosen / memory / irq / dice / path resolve |
| `kernel_elf_parse` | `fuzz_targets/kernel_elf_parse.rs` | `kernel-elf-loader` | boot `KernelElf::parse` + segments |
| `user_elf_parse` | `fuzz_targets/user_elf_parse.rs` | `kernel-elf-parser` | `ELFHeadersBuilder` → `ELFParser` |
| `kbpf_vm_run` | `fuzz_targets/kbpf_vm_run.rs` | `kbpf` | interpreter with instruction limit |
| `rsext4_extent_parse` | `fuzz_targets/rsext4_extent_parse.rs` | `rsext4` | extent tree node parse |
| `rsext4_dirent_parse` | `fuzz_targets/rsext4_dirent_parse.rs` | `rsext4` | dirent + block iterator |
| `rsext4_gdt_walk` | `fuzz_targets/rsext4_gdt_walk.rs` | `rsext4` | GDT table walk |
| `rsext4_sb_decode` | `fuzz_targets/rsext4_sb_decode.rs` | `rsext4` | superblock + `is_valid` |
| `rsext4_inode_decode` | `fuzz_targets/rsext4_inode_decode.rs` | `rsext4` | 128/256-byte inode |
| `firmware_handoff_memmap` | `fuzz_targets/firmware_handoff_memmap.rs` | `firmware-handoff` | EFI boot memmap |
| `fs9p_dirent` | `fuzz_targets/fs9p_dirent.rs` | `fs9p` (`fuzz`) | 9P dirent / readdir.L |
| `fs9p_path` | `fuzz_targets/fs9p_path.rs` | `fs9p` (`fuzz`) | path normalize / split |
| `tee_sm2_sec1` | `fuzz_targets/tee_sm2_sec1.rs` | `tee_crypto` | SM2 SEC1 pubkey validate |
| `tee_rsa_der` | `fuzz_targets/tee_rsa_der.rs` | `tee_crypto` | RSA PKCS#1 / SPKI DER |

Planned surfaces beyond these targets: see [`docs/fuzz-entrypoints.md`](../docs/fuzz-entrypoints.md).
