---
title: "快速开始"
description: "在 QEMU 中启动 AArch64 内核"
---

# 快速开始

本篇使用 Linux 主机，在 QEMU 中运行 AArch64 版本的 X-kernel。目标是完成配置、构建和启动，并识别系统是否进入用户空间。本篇不包含实机安装，也不需要写入物理磁盘。

![从源码、配置、构建到 QEMU 启动的流程](../../assets/docs/boot-zh-CN.svg)

## 1. 准备主机和源码

下面的系统软件安装命令适用于 Debian / Ubuntu。其他发行版请安装对应软件包。

```bash
sudo apt update
sudo apt install git build-essential pkg-config curl xz-utils unzip python3 qemu-system-arm

git clone https://gitee.com/openkylin/x-kernel.git
cd x-kernel
```

通过 [Rust 官方安装说明](https://www.rust-lang.org/tools/install)安装 rustup。仓库根目录的 `rust-toolchain.toml` 会指定 Rust 版本、组件和目标架构，不要自行替换成系统自带的 Rust。

```bash
rustup show
rustc --version
qemu-system-aarch64 --version
```

首次执行 Rust 命令时，rustup 可能需要下载项目工具链。以当前检出的 `rust-toolchain.toml` 为准，不依赖文档中固定的版本号。

如果构建用户态程序或依赖的 C 代码，需要 AArch64 musl 交叉工具链。仓库 README 提供了工具链下载地址；安装后将其 `bin` 目录加入 `PATH`：

```bash
export PATH="/path/to/aarch64-linux-musl-cross/bin:$PATH"
aarch64-linux-musl-gcc --version
```

将示例路径替换为实际安装目录。后续命令均在 X-kernel 仓库根目录执行。

## 2. 选择平台配置

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

`cp` 选择平台的最小配置，`make defconfig` 将它展开为完整 `.config`。复制操作会覆盖已有配置；需要保留自己的配置时，先另存一份。

初次运行先保持默认配置。构建成功后，再使用 `make menuconfig` 探索可配置选项。

## 3. 准备根文件系统

```bash
make rootfs
```

内核和根文件系统是不同的构建输入：内核提供系统能力，rootfs 提供用户态程序和文件。该命令下载预构建镜像，需要可用的网络和本地磁盘空间。

如需安装仓库自带的用户态程序，可以在准备镜像后执行 `make uapps`；第一次启动不必从这一步开始。不要随意删除或覆盖已经保存数据的镜像。

## 4. 构建并运行

```bash
make build
make run
```

`make build` 根据当前平台配置构建内核，并在 `target/xkmake/<platform>/<profile>/` 下生成 bundle。`make run` 使用当前配置启动 QEMU，也可能检查或更新构建产物。

具体日志随提交和配置变化。判断启动成功时，应关注启动日志是否继续进入用户空间，而不是逐字比较字符画或版本号。使用提供交互 shell 的 rootfs 时，可以尝试：

```sh
pwd
ls /
```

应看到工作目录和根目录内容；shell 提示符的用户名、主机名与路径由镜像决定。

使用终端串口方式运行 QEMU 时，先按 **Ctrl+A**，松开后按 **X** 退出 QEMU。不要把这两个键同时按下。

## 常见阻碍

| 现象 | 先检查什么 |
| --- | --- |
| `qemu-system-aarch64: command not found` | QEMU 软件包是否安装，命令是否在 PATH 中 |
| Rust 目标或组件缺失 | 是否通过 rustup 使用仓库指定工具链，下载是否成功 |
| 找不到交叉编译器 | musl 工具链的 `bin` 是否加入 PATH |
| rootfs 下载失败或找不到镜像 | 查看 `make rootfs` 的第一条错误，确认下载地址、网络与磁盘空间 |
| 配置错误或启动架构不匹配 | 确认使用 AArch64 的 defconfig，重新执行 `make defconfig` |

不要用裸 `cargo build` 替代本文的 Makefile 流程；项目还需要 Kconfig 生成的配置。仍无法解决时，按[获取帮助](getting-help.zh.md)中的清单提交日志。

## 本文依据

命令依据仓库 README、平台 defconfig 与 Makefile/xkmake 流程整理。仓库持续演进，反馈问题时请附 `git rev-parse HEAD` 的结果。本轮文档编写没有重新执行完整内核构建，不能将本文视为所有主机环境下的测试报告。
