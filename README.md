# XHyper

XHyper 是基于 X-Kernel 裁剪得到的 Type-1 虚拟机基座。内核直接运行在
AArch64 EL2/VHE，保留启动、架构抽象、内存管理、中断、时钟、SMP、设备驱动
和 RR 调度器，并在 `kruntime` 初始化完成后进入 `entry`。

当前代码不包含 Linux syscall ABI、POSIX、用户进程、文件系统、网络协议栈、
TEE 和 VM/vCPU 业务层。驱动与多架构底层代码仍然保留，当前默认配置和验证
目标为 AArch64 QEMU virt。

## 环境要求

构建和运行至少需要：

- Git、GNU Make；
- `rust-toolchain.toml` 指定的 Rust nightly 工具链；
- Rust target `aarch64-unknown-none-softfloat`；
- `cargo-binutils` 和 `rust-objcopy`；
- `qemu-system-aarch64`。

在 Debian/Ubuntu 上可安装 QEMU：

```bash
sudo apt install qemu-system-arm
rustup target add aarch64-unknown-none-softfloat
cargo install cargo-binutils
rustup component add llvm-tools-preview
```

## 构建与启动

所有构建、运行和测试命令执行前，都应先从平台 defconfig 生成 `.config`：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

构建内核：

```bash
make build
```

产物位于：

```text
target/xkmake/kplat-aarch64/release/
├── bundle.toml
├── kernel.bin
└── kernel.elf
```

使用 QEMU 启动：

```bash
make run XKMAKE_ARGS='--no-block --no-net'
```

启动完成后会展示 build info、Build ID 和 SMP 数量，并进入串口 shell：

```text
XHyper EL2 runtime initialized
xhyper> echo hello
hello
xhyper> exit
```

shell 目前仅支持：

- `echo <内容>`：输出参数；
- `exit`：关闭虚拟机。

可通过 `XKMAKE_ARGS` 调整 QEMU 内存和 CPU 数量：

```bash
make run XKMAKE_ARGS='--no-block --no-net --memory 2g --smp 2'
```

## 目录结构

```text
api/        顶层 feature 聚合与公共接口
arch/       CPU、HAL、IPI 和各架构底层实现
boot/       启动协议、引导入口和镜像构建信息
core/       EL2 核心运行时、日志与时间基础设施
drivers/    控制台、中断、时钟、PCI、VirtIO 等设备驱动
entry/      XHyper 运行入口、构建信息展示和最小 shell
io/         驱动仍需使用的底层 DMA 支持
mm/         分配器、页表、内核地址空间和映射支持
platforms/  平台适配、Kconfig 和 defconfig
task/       内核任务、同步原语、轮询和 RR 调度器
util/       通用工具、宏、错误类型和内核测试框架
xtask/      XKMake 构建、镜像打包和 QEMU 编排工具
docs/       设计文档、开发规范和 AI 工作流
```

## 测试与检查

运行 QEMU 内核单元测试：

```bash
make unittest XKMAKE_ARGS='--no-block --no-net'
```

限制到指定 crate：

```bash
make unittest UNITTEST_CRATE=memspace XKMAKE_ARGS='--no-block --no-net'
```

无块设备模式不会生成 guest 覆盖率文件，因此会跳过覆盖率提取。需要覆盖率报告
时必须提供可用的磁盘镜像和 `debugfs`、`rust-profdata`、`rust-cov`。

运行静态检查和格式化：

```bash
make clippy
cargo +nightly-2026-03-08 fmt --all
```

建议提交前至少执行：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
make unittest XKMAKE_ARGS='--no-block --no-net'
cargo +nightly-2026-03-08 fmt --all
```

## License

本项目使用 Apache License 2.0，详见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。
