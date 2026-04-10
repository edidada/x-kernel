# x-kernel aarch64-crosvm-virt 在 Android 设备上启动记录

**日期**: 2026-04-10
**分支**: `pr_111`
**HEAD**: `5124e0c6 fix x86 clippy`
**目标设备**: Android 设备 `10.88.21.42`（通过 ADB 连接）
**Hypervisor**: `/apex/com.android.virt/bin/crosvm`（AVF 版本）

## 1. 环境准备

### 1.1 连接 Android 设备

```bash
adb connect 10.88.21.42
adb devices     # 确认 10.88.21.42:5555 device
```

### 1.2 确认设备上的 crosvm

```bash
adb shell 'su 0 sh -c "which crosvm"'
# 输出: /apex/com.android.virt/bin/crosvm
```

> 注意：这是 Android 虚拟化框架（AVF）中的 crosvm fork，其 CLI
> 参数与主线 crosvm 有差异（例如不支持 `--version`）。

## 2. 构建 kernel（带 filesystem 的 aarch64-crosvm-virt 配置）

### 2.1 使用平台默认 config 并提高日志等级

```bash
cp platforms/aarch64-crosvm-virt/defconfig .config
```

将 `.config` 中的日志等级由 `LOG_LEVEL_WARN=y` 改为 `LOG_LEVEL_DEBUG=y`：

```
LOG_LEVEL_DEBUG=y
# LOG_LEVEL_ERROR is not set
# LOG_LEVEL_INFO is not set
# LOG_LEVEL_TRACE is not set
# LOG_LEVEL_WARN is not set
```

> 注意：`make oldconfig` 在这个版本会把 `.config` 清空（只留 `ARCH="aarch64"`），
> 因此本次流程是手动编辑 `.config`、直接调用 `make build`，不调用 `oldconfig`。

### 2.2 构建 kernel 镜像

```bash
make build
```

构建产物：

- `xkernel_aarch64-crosvm-virt.elf`（约 55 MB，含 DWARF 调试信息）
- `xkernel_aarch64-crosvm-virt.bin`（约 55 MB，用作 crosvm `--kernel` 输入）

### 2.3 准备根文件系统镜像

```bash
make rootfs
```

该命令会从 `https://gitee.com/openkylin/x-kernel-image/releases/download/20260302/`
下载 `rootfs-aarch64.img.xz`，解压为 `rootfs-aarch64.img` 并拷贝到 `disk.img`：

- `disk.img`：1.0 GB 的 ext4 文件系统镜像

## 3. 推送产物到 Android 设备

目标目录选定为 `/data/local/tmp/jose`，避免覆盖设备上已有文件。

```bash
# 在设备上创建目录
adb shell 'su 0 sh -c "mkdir -p /data/local/tmp/jose && chmod 777 /data/local/tmp/jose"'

# 推送 kernel 镜像（使用规范文件名 kernel.bin）
adb push xkernel_aarch64-crosvm-virt.bin /data/local/tmp/jose/kernel.bin

# 推送根文件系统
adb push disk.img /data/local/tmp/jose/disk.img
```

推送结果：

```
xkernel_aarch64-crosvm-virt.bin: 1 file pushed (57225728 bytes)
disk.img:                       1 file pushed (1073741824 bytes)
```

## 4. 启动 crosvm

本次实验尝试了两种启动方式，关键区别只在有无 `--protected-vm-without-firmware`
这一 pKVM 相关开关。

通用参数说明：

- `--disable-sandbox`：在 Android shell 环境下禁用 minijail 沙箱
- `--cpus 1`：与设备上原 strace 里成功启动一致（`--cpus 2` 症状相同）
- `--mem 2048`：与原 `vm_config.json` 保持一致
- `--block <path>`：挂载 ext4 根文件系统
- 最后一个位置参数为 kernel 镜像
- `--log-level debug`：打开 crosvm 自身的 DEBUG 日志
- `--protected-vm-without-firmware`：使用 AVF/pKVM 的 protected VM 模式但
  不加载 `pvmfw`，适合直接把 x-kernel 作为 guest 跑

通用的 logcat 观察手段：

```bash
adb shell 'su 0 sh -c "logcat -c"'                        # 启动前清 logcat
# crosvm 启动后另开终端：
adb shell 'su 0 sh -c "logcat -d -s crosvm:*"' | tail -80
```

## 5. 运行日志与崩溃/卡住现象

### 5.1 加上 `--protected-vm-without-firmware`（protected VM 模式）

#### 5.1.1 使用的命令

```bash
adb shell 'su 0 timeout 30 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox \
    --protected-vm-without-firmware \
    --cpus 1 \
    --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

#### 5.1.2 完整输出

```
[2026-04-10T06:54:30.844556620+00:00 DEBUG crosvm::crosvm::sys::linux] creating hypervisor: Kvm { device: Some("/dev/kvm") }
[2026-04-10T06:54:30.845711760+00:00 INFO  crosvm::crosvm::sys::linux::device_helpers] Trying to attach block device: /data/local/tmp/jose/disk.img
[2026-04-10T06:54:30.845825260+00:00 INFO  disk] disk size 1073741824
[boot] entered primary switched cpu=0 dtb=0xffe00000 kimage_voffset=0xffff7fff80000000
[boot] boot linear RAM map extended from DT
[boot] handoff to kruntime boot_info=0xffff800003692b58
[boot] kruntime primary start cpu=0 boot_info=0xffff800003692b58
[boot] firmware: DTB at 0xffe00000
[boot] firmware: DTB capture region=0xffe00000..0x100000000
[boot] firmware: chosen stdout-path=/U6_16550A@3f8
[boot] firmware: chosen bootargs=panic=-1 console=ttyS0
[boot] firmware: using chosen bootargs as cmdline=panic=-1 console=ttyS0
[boot] kruntime memory regions ready
[boot] kruntime allocator ready
[boot] memory space map ready
panicked at platforms/aarch64-crosvm-virt/src/psci.rs:45:9:
explicit panic
Backtrace:
  <no frames captured>

[2026-04-10T06:54:30.934321200+00:00 INFO  crosvm::crosvm::sys::linux::vcpu] system shutdown event on vcpu 0
[2026-04-10T06:54:30.934477680+00:00 INFO  crosvm::crosvm::sys::linux] vcpu requested shutdown
[2026-04-10T06:54:30.934494060+00:00 ERROR crosvm::crosvm::sys::linux::vcpu] failed to send VcpuControl: sending on a closed channel
[2026-04-10T06:54:30.969863480+00:00 INFO  crosvm] exiting with success
```

#### 5.1.3 崩溃位置的代码定位

与 5.2 的非保护模式不同，开启 protected VM 后内核已经顺利完成：

1. `firmware::init`（含 DT 解析、cmdline 装配）
2. `khal::mem::init` → 打印 `kruntime memory regions ready`
3. `init_allocator` → 打印 `kruntime allocator ready`
4. `memspace::init_memory_management` → 打印 `memory space map ready`

紧接着的 `core/kruntime/src/lib.rs:172` 是 `khal::early_driver_init();`，
它调用平台侧的 `BootHandlerImpl::early_driver_init`
（`platforms/aarch64-crosvm-virt/src/init.rs:26-38`）。里面第一个做 MMIO
iomap 的是 `console_driver::init_from_device_tree()`，调用链是：

```
console_driver::init_from_device_tree()
  -> memspace::iomap_device(paddr, size, "console-ns16550" / "console-pl011")
     (mm/memspace/src/iomap.rs:84)
  -> kplat::mmio::prepare(start, span)
  -> PlatformMmioIf::prepare            (platforms/aarch64-crosvm-virt/src/psci.rs:86)
  -> do_xmap_granules(paddr, size)       (psci.rs:65)
  -> __invoke_mmioguard(phys_addr, ..)   (psci.rs:37)
  -> psci_hvc_call(MMIO_GUARD_MAP, ..)
     ↳ HVC 返回 result != 0
  -> panic!()                            (psci.rs:45) ← 当前崩溃点
```

触发点就是 `__invoke_mmioguard` 里这段：

```rust
let (result, done) = psci_hvc_call(func_id, phys_addr, 1, 0);
if result != 0 {
    panic!();
}
```

也就是说，**第一次**想通过 KVM MMIO guard 为某个设备（几乎肯定是 UART
所在的 `0x3f8` 那一页）建立映射时，pKVM 返回了非零错误码。DT 中的
`/U6_16550A@3f8` 把 UART 放在物理地址 `0x3f8`，经过 `iomap_device` 的
`align_down_4k` 之后，实际请求映射的是 `0x0..0x1000` 这一页——这个范围
基本不会在 pKVM 的 MMIO guard 白名单里。

### 5.2 不加 `--protected-vm-without-firmware`（普通 VM 模式）

#### 5.2.1 使用的命令

```bash
adb shell 'su 0 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox \
    --cpus 1 \
    --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

#### 5.2.2 完整输出

在 30 秒超时内仅输出以下 12 行后完全卡住：

```
[2026-04-10T06:49:17.304664360+00:00 DEBUG crosvm::crosvm::sys::linux] creating hypervisor: Kvm { device: Some("/dev/kvm") }
[2026-04-10T06:49:17.305822300+00:00 INFO  crosvm::crosvm::sys::linux::device_helpers] Trying to attach block device: /data/local/tmp/jose/disk.img
[2026-04-10T06:49:17.305898760+00:00 INFO  disk] disk size 1073741824
[boot] entered primary switched cpu=0 dtb=0xffe00000 kimage_voffset=0xffff7fff80000000
[boot] boot linear RAM map extended from DT
[boot] handoff to kruntime boot_info=0xffff800003692b58
[boot] kruntime primary start cpu=0 boot_info=0xffff800003692b58
[boot] firmware: DTB at 0xffe00000
[boot] firmware: DTB capture region=0xffe00000..0x100000000
[boot] firmware: chosen stdout-path=/U6_16550A@3f8
[boot] firmware: chosen bootargs=panic=-1 console=ttyS0
[boot] firmware: using chosen bootargs as cmdline=panic=-1 console=ttyS0
```

对应 logcat 侧只捕获到 crosvm 自身的三行（与 stdout 前三行一致）：

```
04-10 14:42:54.829 18578 18578 D crosvm  : creating hypervisor: Kvm { device: Some("/dev/kvm") }
04-10 14:42:54.830 18578 18578 I crosvm  : Trying to attach block device: /data/local/tmp/jose/disk.img
04-10 14:42:54.830 18578 18578 I crosvm  : disk size 1073741824
```

#### 5.2.3 卡住位置的代码定位

最后一条 `[boot]` 输出来自 `arch/khal/src/firmware/init.rs:50`：

```rust
bootln!("firmware: using chosen bootargs as cmdline={}", cmdline);
```

随后的控制流在 `core/kruntime/src/lib.rs` 中：

```rust
// 162: khal::firmware::init(boot_info);            // <-- 其内部最后一条已打印
// 163: khal::percpu::init_primary(cpu_id);
// 164: kcpu::init_trap();
// 165: khal::mem::init(boot_info);                 // 会调用 init_memory_description
// 166: bootln!("kruntime memory regions ready");   // <-- 从未打印出来
```

也就是说，卡死发生在 `firmware::init` 返回之后、`kruntime memory regions ready`
之前的某处。结合 5.1 的结果可以进一步推断：`khal::mem::init` 会通过
`kplat::boot::prepare_boot_memory` 调用平台侧的 `map_kvm_guarded_mmio`
→ `kvm_guard_granule_init`（`psci.rs:31`），而后者无条件地发起
`ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_INFO` HVC 调用，并
`assert_eq!(guard_has_range, 0x1)`。在普通 VM 下这个 KVM vendor hyp
接口本就不存在，HVC 的行为未定义，表现为卡死（没有进一步输出）。

## 6. 对照实验：设备上原有的 kernel.bin 可以正常启动到文件系统

在同样的 crosvm 参数下运行设备上 4 月 8 日的旧 kernel：

```bash
adb shell 'su 0 timeout 25 /apex/com.android.virt/bin/crosvm run \
    --disable-sandbox --cpus 1 --mem 2048 \
    --block /data/local/tmp/disk.img \
    /data/local/tmp/kernel.bin'
```

输出节选（完整日志见 `/tmp/old_kernel_full.log`）：

```
[kruntime:162] Found physcial memory regions:
[kruntime:164]   [PA:0x3f8, PA:0x13f8) mmio (READ | WRITE | DEVICE | RESERVED)
...
[kruntime:253] Initialize global memory allocator...
[memspace:84] Initialize virtual memory management...
[kruntime:201] Initialize platform devices...
[aarch64_crosvm_virt::init:51] cpu_id 0
[aarch64_crosvm_virt::gicv3:38] Initialize GICv3... from 0xffff00003fff0000 0xffff00003ffd0000
[aarch64_crosvm_virt::init:57] set UART IRQ 32 as edge trigger
[ktask::api:94] Initialize scheduling...
[kdriver:107] Initialize device drivers...
[virtio:170] PCI virtio device at DeviceFunction { bus: 0, device: 1, function: 0 }: IRQ = 36
[kvirtiodrivers::device::blk:64] found a block device of size 65536KB
[kdriver::bus::pci:141] registered a new Block device at 00:01.0: "virtio-blk"
[kfs:40] Initialize filesystem subsystem...
[kfs:57]   use block device 0: "virtio-blk"
[kfs:60]   filesystem type: "ext4"
[knet:67] Initialize network subsystem...
[knet:105]   No network device found!
[knet:110] Device: lo
[knet:130] Initialize vsock subsystem...
[fbdevice:20] Initialize framebuffer subsystem...
[inputdev:23] Initialize input subsystem...
[aarch64_peripherals::psci:107] Starting CPU 1 ON ...
[aarch64_peripherals::psci:110] failed to boot CPU 1 (InvalidParams)
```

该旧 kernel 能够完整地：

1. 枚举物理内存、建立虚拟内存映射
2. 初始化 GICv3、调度器、PCI/virtio-blk
3. 挂载 ext4 文件系统
4. 只在尝试启动第二颗 CPU 时 PSCI 返回 `InvalidParams`（与 pKVM 下
   PSCI CPU_ON 的已知问题一致）

这证明 crosvm、pKVM、disk.img 等运行环境本身是正常的。

## 7. 结论

- 环境本身工作正常：设备上早前构建的 `kernel.bin` 可以在同样的 crosvm
  命令下跑到文件系统挂载阶段。
- **`--protected-vm-without-firmware` 是正确的启动姿势**：不开这个开关
  时，当前分支的镜像在 `firmware::init` 之后立刻卡死，是因为
  `kvm_guard_granule_init` 里无条件做了 KVM vendor hyp HVC 调用，非
  protected VM 下该接口不存在，行为未定义。
- 开了 protected VM 之后，内核顺利跑到 `early_driver_init`，然后在第
  一次 MMIO iomap 时 panic 在
  `platforms/aarch64-crosvm-virt/src/psci.rs:45`——MMIO guard HVC 对
  请求映射的页（很可能是 UART `0x3f8` 对应的 `0x0..0x1000`）返回非 0
  错误码，代码直接 `panic!()`。
- 下一步建议：
  - 查看 `console_driver::init_from_device_tree` 解析出的 UART 物理地
    址/大小，以及 `iomap_device` 实际向 `__invoke_mmioguard` 传入的
    对齐后 `(paddr, size)`；
  - 在 `__invoke_mmioguard` 的 panic 前把 `func_id / phys_addr /
    result / done` 打印出来，或者把 `panic!()` 改成 `log::error!` +
    返回错误，至少能让 boot 继续跑出真正的第一条错误；
  - 检查设备上 pKVM 暴露给 non-firmware protected VM 的 MMIO guard
    白名单，确认 UART `0x3f8` 所在页是否被允许（怀疑 DT 里 UART 节点
    需要配成标准的 MMIO 基址，而不是 `0x3f8` 这样的 PIO 风格地址）。
