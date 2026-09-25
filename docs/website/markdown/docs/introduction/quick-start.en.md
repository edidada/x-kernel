---
title: "Quick Start"
description: "Boot AArch64 on QEMU"
---

# Quick Start

Use a Linux host to run the AArch64 version of X-kernel in QEMU. This guide takes you through configuration, building, and booting, then helps you recognize entry into user space. It does not install onto physical hardware or write to a physical disk.

![Source, configuration, build, and QEMU boot workflow](../../assets/docs/boot-en.svg)

## 1. Prepare the host and source

These package commands target Debian / Ubuntu. Install equivalent packages on other distributions.

```bash
sudo apt update
sudo apt install git build-essential pkg-config curl xz-utils unzip python3 qemu-system-arm

git clone https://gitee.com/openkylin/x-kernel.git
cd x-kernel
```

Install rustup using the [official Rust installation instructions](https://www.rust-lang.org/tools/install). The repository's `rust-toolchain.toml` selects the Rust version, components, and compilation targets. Do not substitute the distribution's Rust package for the selected toolchain.

```bash
rustup show
rustc --version
qemu-system-aarch64 --version
```

The first Rust command may trigger toolchain downloads. Follow the `rust-toolchain.toml` in your checkout rather than a version number copied from documentation.

Building user programs or dependent C code requires an AArch64 musl cross-toolchain. Download links are listed in the repository README. After installing it, add its `bin` directory to `PATH`:

```bash
export PATH="/path/to/aarch64-linux-musl-cross/bin:$PATH"
aarch64-linux-musl-gcc --version
```

Replace the example path with your installation directory. Run the remaining commands from the X-kernel repository root.

## 2. Select the platform configuration

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

The copy selects a minimal platform configuration; `make defconfig` expands it into a complete `.config`. Copying overwrites an existing configuration, so save any custom configuration first.

Keep defaults for the first boot. After a successful build, use `make menuconfig` to explore configuration options.

## 3. Prepare the root filesystem

```bash
make rootfs
```

The kernel and root filesystem are separate inputs: the kernel provides system capabilities, while the rootfs supplies user programs and files. This command downloads a prebuilt image and needs network access and available disk space.

To install the repository's user programs, optionally run `make uapps` after preparing the image. This is not the starting point for a first boot. Avoid deleting or overwriting images containing data you want to keep.

## 4. Build and run

```bash
make build
make run
```

`make build` builds the configured kernel and creates a bundle under `target/xkmake/<platform>/<profile>/`. `make run` starts QEMU for the current configuration and may also check or update build artifacts.

Logs vary with revision and configuration. Look for progress into user space rather than an exact banner or version string. With a rootfs that provides an interactive shell, try:

```sh
pwd
ls /
```

You should see the working directory and root-directory entries. The username, hostname, and path in the shell prompt depend on the image.

When QEMU runs with a terminal serial console, press **Ctrl+A**, release the keys, then press **X** to exit QEMU. This is a sequence, not one simultaneous key combination.

## Common obstacles

| Symptom | First check |
| --- | --- |
| `qemu-system-aarch64: command not found` | QEMU package installation and PATH |
| Missing Rust target or component | rustup selected the repository toolchain and downloads completed |
| Cross-compiler not found | The musl toolchain's `bin` directory is on PATH |
| Rootfs download fails or image is missing | First error from `make rootfs`, download URL, network, and disk space |
| Configuration errors or architecture mismatch | AArch64 defconfig was selected and `make defconfig` completed |

Do not replace this Makefile workflow with a bare `cargo build`: the project also depends on Kconfig-generated configuration. If the issue remains, follow [Getting Help](getting-help.en.md) to collect useful diagnostics.

## Basis for this guide

Commands were checked against the repository README, platform defconfig, and Makefile/xkmake workflow. Include `git rev-parse HEAD` when reporting a problem because the repository evolves. A full kernel rebuild was not repeated during this documentation update; this guide is not a test report for every host environment.
