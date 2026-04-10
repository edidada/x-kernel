# 在 Android AVF crosvm 下启动 aarch64-crosvm-virt —— MMIO guard 绕过迭代

**日期**: 2026-04-10
**分支**: `main`（HEAD `6de5d8ce !111 refactor irq`）+ 本地未提交改动
**平台**: `aarch64-crosvm-virt`
**目标**: 让 x-kernel 在 Android AVF crosvm 下以 `--protected-vm-without-firmware`
          方式启动

本文档承接 `doc/crosvm-android-boot-log.md` 中记录的 protected VM panic
于 `platforms/aarch64-crosvm-virt/src/psci.rs:45`（`__invoke_mmioguard` 里
HVC 返回非零）的问题，记录尝试绕过 pKVM MMIO guard 的第一轮迭代。

## 1. 本次代码改动

用户在 `platforms/aarch64-crosvm-virt/src/init.rs` 中把 `prepare_boot_memory`
早期钩子调用的 `kvm_guard_granule_init()` 注释掉（连带封装函数一起），
想看看跳过"查询 pKVM MMIO guard granule size" 这一步能否越过之前的
panic：

```diff
--- a/platforms/aarch64-crosvm-virt/src/init.rs
+++ b/platforms/aarch64-crosvm-virt/src/init.rs
@@ -7,16 +7,16 @@ use kbuild_config::PSCI_METHOD;
 use kplat::boot::{BootHandler, BootInfo};
 use log::*;

-fn map_kvm_guarded_mmio() {
-    crate::psci::kvm_guard_granule_init();
-}
+// fn map_kvm_guarded_mmio() {
+//     crate::psci::kvm_guard_granule_init();
+// }

 struct BootHandlerImpl;
 #[impl_dev_interface]
 impl BootHandler for BootHandlerImpl {
     fn prepare_boot_memory(boot_info: &BootInfo) {
-        map_kvm_guarded_mmio();
-        let _ = boot_info;
+        // map_kvm_guarded_mmio();
+        // let _ = boot_info;
     }

     fn firmware_init(_boot_info: &BootInfo) {
```

关键观察：`PlatformMmioIf::prepare` → `do_xmap_granules` → `__invoke_mmioguard`
这条 MMIO guard 调用链本身**没有改动**。只是跳过了"把 `GUARD_GRANULE`
`call_once` 填好"这一步。`GUARD_GRANULE` 是 `psci.rs` 里的 `Once<usize>`
全局变量，由 `kvm_guard_granule_init()` 里写入。

## 2. 构建步骤

```bash
# 1) 恢复 crosvm 平台默认配置
cp platforms/aarch64-crosvm-virt/defconfig .config

# 2) 手动把 .config 里的日志等级改成 DEBUG
#    LOG_LEVEL_DEBUG=y
#    # LOG_LEVEL_WARN is not set
#    其它 LOG_LEVEL_* 保持 "is not set"
#    （注：不要用 `make oldconfig`，它会把 .config 清空成只剩 ARCH）

# 3) 编译
make build
```

构建产物：

- `xkernel_aarch64-crosvm-virt.elf`（约 55 MB，带 DWARF）
- `xkernel_aarch64-crosvm-virt.bin`（57 221 632 字节）

## 3. 推送到 Android 设备

```bash
adb connect 10.88.21.42        # 如果需要
adb devices                    # 确认 10.88.21.42:5555 device
adb push xkernel_aarch64-crosvm-virt.bin /data/local/tmp/jose/kernel.bin
```

注：`disk.img` 这次没有重新推送，复用之前那份 1 GB ext4 镜像
(`/data/local/tmp/jose/disk.img`)。

## 4. 启动命令（protected VM without firmware）

```bash
adb shell 'su 0 sh -c "logcat -c"'              # 清 logcat

adb shell 'su 0 timeout 40 /apex/com.android.virt/bin/crosvm \
    --log-level debug run \
    --disable-sandbox \
    --protected-vm-without-firmware \
    --cpus 1 \
    --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

## 5. 运行日志（完整）

```
[2026-04-10T07:34:50.813753200+00:00 DEBUG crosvm::crosvm::sys::linux] creating hypervisor: Kvm { device: Some("/dev/kvm") }
[2026-04-10T07:34:50.814996480+00:00 INFO  crosvm::crosvm::sys::linux::device_helpers] Trying to attach block device: /data/local/tmp/jose/disk.img
[2026-04-10T07:34:50.815072940+00:00 INFO  disk] disk size 1073741824
[boot] entered primary switched cpu=0 dtb=0xffe00000 kimage_voffset=0xffff7fff80000000
[boot] boot linear RAM map extended from DT
[boot] handoff to kruntime boot_info=0xffff800003691b58
[boot] kruntime primary start cpu=0 boot_info=0xffff800003691b58
[boot] firmware: DTB at 0xffe00000
[boot] firmware: DTB capture region=0xffe00000..0x100000000
[boot] firmware: chosen stdout-path=/U6_16550A@3f8
[boot] firmware: chosen bootargs=panic=-1 console=ttyS0
[boot] firmware: using chosen bootargs as cmdline=panic=-1 console=ttyS0
[boot] kruntime memory regions ready
[boot] kruntime allocator ready
[boot] memory space map ready
panicked at platforms/aarch64-crosvm-virt/src/psci.rs:66:52:
called `Option::unwrap()` on a `None` value
Backtrace:
  <no frames captured>

[2026-04-10T07:34:50.941095420+00:00 INFO  crosvm::crosvm::sys::linux::vcpu] system shutdown event on vcpu 0
[2026-04-10T07:34:50.941254220+00:00 INFO  crosvm::crosvm::sys::linux] vcpu requested shutdown
[2026-04-10T07:34:50.941311980+00:00 ERROR crosvm::crosvm::sys::linux::vcpu] failed to send VcpuControl: sending on a closed channel
[2026-04-10T07:34:50.978851100+00:00 INFO  crosvm] exiting with success
```

## 6. 故障点分析

Panic 位置是 `platforms/aarch64-crosvm-virt/src/psci.rs:66:52`：

```rust
/// Map a physical MMIO range via the KVM guard-granule interface.
pub fn do_xmap_granules(phys_addr: usize, size: usize) {
    let nr_granules = size / { GUARD_GRANULE.get().unwrap() };   // ← 66:52
    let ret = __do_xmap_granules(phys_addr, nr_granules, true);
    assert_eq!(ret, nr_granules);
}
```

`GUARD_GRANULE: Once<usize>` 是一个全局一次性初始化的变量，原本只在
`kvm_guard_granule_init()` 里通过 `GUARD_GRANULE.call_once(|| guard_granule)`
写入。本次改动注释掉了 `prepare_boot_memory` 对它的调用，所以
`GUARD_GRANULE.get()` 始终返回 `None`，`.unwrap()` 直接 panic。

与前一次 panic（`psci.rs:45` 的 `panic!()`）的对比：

| 项                  | 前一次                             | 本次                               |
| ------------------- | ---------------------------------- | ---------------------------------- |
| Panic 位置          | `psci.rs:45 __invoke_mmioguard`    | `psci.rs:66 do_xmap_granules`      |
| 触发原因            | HVC 返回 `result != 0`             | `GUARD_GRANULE` 未被 `call_once`   |
| 上一条 boot 日志    | `[boot] memory space map ready`    | `[boot] memory space map ready`    |
| 距 boot 完成的距离  | 几乎相同                           | 几乎相同                           |

**结论**：两次 panic 走的是完全相同的调用链：

```
khal::early_driver_init
  → platforms::aarch64-crosvm-virt::init::BootHandlerImpl::early_driver_init
  → console_driver::init_from_device_tree()
  → memspace::iomap_device(paddr=0x0, size=0x1000, "console-ns16550")
    (mm/memspace/src/iomap.rs:84)
  → kplat::mmio::prepare(start, span)
  → PlatformMmioIf::prepare                 (psci.rs:86)
  → do_xmap_granules(paddr, size)            (psci.rs:65)  ← 本次在这里炸
    → __invoke_mmioguard(phys_addr, ..)      (psci.rs:37)  ← 上次在这里炸
```

也就是说，只注释 `prepare_boot_memory` 的调用点不够——只要任何一个
驱动调用 `memspace::iomap_device`，都会经由 `kplat::mmio::prepare` 进
入这条完整的 MMIO guard 链。要彻底绕开，需要把 `PlatformMmioIf::prepare`
本身改成 no-op（见下一节）。

## 7. 下一步修复计划

把 `platforms/aarch64-crosvm-virt/src/psci.rs` 里的
`PlatformMmioIf::prepare` 短路成 no-op：

```rust
#[impl_dev_interface]
impl PlatformMmioIf for MmioPlatformImpl {
    fn prepare(_paddr: usize, _size: usize) -> kerrno::KResult {
        // 非 protected VM / 无 pKVM MMIO guard：普通 KVM 已经把 stage-2
        // 建好了，iomap_device 继续走后续的 map_linear 把 MMIO 页塞进
        // 内核虚拟地址空间即可。
        Ok(())
    }
}
```

这样 `iomap_device` 就不再调用任何 HVC，直接进入 `kernel_aspace.map_linear`，
`early_driver_init` 也就能继续往前跑。下一个故障点有几种可能：

1. **UART 物理地址不对**：DT 里的 `/U6_16550A@3f8` 经 `align_down_4k`
   变成映射 `0x0..0x1000`，真正的 UART 寄存器（字节 offset `0x3f8`）
   对应的虚拟地址是 `vbase + 0x3f8`。如果 crosvm 的 NS16550 确实在
   物理地址 `0x3f8` 有 MMIO 寄存器，UART 可以正常输出。
2. **stage-2 缺页**：如果 crosvm 没有把 `0x0..0x1000` 这一页映射为
   NS16550 MMIO，访问时会触发 data abort，Boot 再次停住。
3. **顺利到 PCI/virtio-blk 枚举**：与 4 月 8 日那版旧 kernel 的行为一致，
   一路跑到 `kfs: use block device 0 "virtio-blk"` 和文件系统挂载。

无论落到哪一步，都比现在 `psci.rs:66` 的 panic 更有诊断价值。

## 8. 应用修复：`PlatformMmioIf::prepare` 改为 no-op

实际写入 `platforms/aarch64-crosvm-virt/src/psci.rs` 的 diff：

```diff
 #[impl_dev_interface]
 impl PlatformMmioIf for MmioPlatformImpl {
-    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
-        do_xmap_granules(paddr, size);
+    fn prepare(_paddr: usize, _size: usize) -> kerrno::KResult {
+        // Non-protected VM without pvmfw: skip the KVM MMIO guard HVC chain.
+        // Regular KVM already sets up the guest stage-2 mapping, so
+        // iomap_device just needs to continue into map_linear.
         Ok(())
     }
 }
```

注意：

- 故意**没有**恢复用户对 `prepare_boot_memory` 的注释——既然 `do_xmap_granules`
  被短路，`GUARD_GRANULE` 有没有初始化都无所谓，保留用户的改动能减少回归面。
- 没有动 `PlatformDmaIf::{prepare, release}` 里的
  `dma_share_pages / dma_unshare_pages`。那条路径里 HVC 返回非零时只
  打 `log::warn!`、不会 panic（前面旧 kernel 日志里就有大量
  `cannot share/unshare 0xc0...` 警告却能继续跑），所以暂时保留原样。

## 9. 修复后的运行结果

### 9.1 启动命令（同前）

```bash
adb shell 'su 0 sh -c "logcat -c"'
adb shell 'su 0 timeout 45 /apex/com.android.virt/bin/crosvm \
    --log-level debug run \
    --disable-sandbox \
    --protected-vm-without-firmware \
    --cpus 1 \
    --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

### 9.2 完整输出（121 行，截取关键部分）

```
[2026-04-10T07:38:31 DEBUG crosvm] creating hypervisor: Kvm
[2026-04-10T07:38:31 INFO  crosvm] Trying to attach block device: /data/local/tmp/jose/disk.img
[2026-04-10T07:38:31 INFO  disk] disk size 1073741824
[boot] entered primary switched cpu=0 dtb=0xffe00000 kimage_voffset=0xffff7fff80000000
[boot] boot linear RAM map extended from DT
[boot] handoff to kruntime boot_info=0xffff800003691b58
[boot] kruntime primary start cpu=0 boot_info=0xffff800003691b58
[boot] firmware: DTB at 0xffe00000
[boot] firmware: DTB capture region=0xffe00000..0x100000000
[boot] firmware: chosen stdout-path=/U6_16550A@3f8
[boot] firmware: chosen bootargs=panic=-1 console=ttyS0
[boot] firmware: using chosen bootargs as cmdline=panic=-1 console=ttyS0
[boot] kruntime memory regions ready
[boot] kruntime allocator ready
[boot] memory space map ready

                 ++
             *  ***
             ...(openKylin x-kernel LOGO)
             ...

arch = aarch64
platform = aarch64-crosvm-virt
target = aarch64-unknown-none-softfloat
build_mode = release
build_machine = jose@jose-kylin
build_time = 2026-04-10T07:37:51Z
log_level = debug
backtrace = true
smp = 2

Boot at 2026-04-10 15:38:31.014067880 UTC

[81838.804431 kruntime:207] Logging is enabled.
[81838.805008 kruntime:208] Primary CPU 0 started, boot_info = 0xffff800003691b58.
[81838.806230 kalloc:243] expand heap memory: [0xffff0000ffa00000, 0xffff0000ffb00000)
[81838.813275 kalloc:243] expand heap memory: [0xffff000083800000, 0xffff000083c00000)
[81838.833900 kalloc:243] expand heap memory: [0xffff0000ff000000, 0xffff0000ff800000)
[81838.868978 kalloc:243] expand heap memory: [0xffff0000fc000000, 0xffff0000fe000000)
[81839.015862 kalloc:243] expand heap memory: [0xffff000084000000, 0xffff000088000000)
[81839.147915 backtrace::dwarf:144] Initialized addr2line context successfully.
[81839.148825 kruntime:234] Initialize platform devices...
[81839.149463 aarch64_crosvm_virt::init:42] cpu_id 0
[81839.150054 irq_driver::gic::gicv3:30] Initialize GICv3 CPU Interface...
[81839.150886 ktask::api:94] Initialize scheduling...
[81839.151566 ktask::task:170] new task: Task(1, "idle")
[81839.152235 ktask::task:170] new task: Task(3, "gc")
[81839.152851 ktask::api:99]   use Round-robin scheduler.
[81839.153533 kdriver:132] Initialize device drivers...
[81839.154156 kdriver:133]   device model: static
[81839.154781 pci:81] PCI host from device tree: cam=Cam ecam=0x10000 size=0x1000000 bus_range=0x0..=0x0
[81839.155842 pci:157] PCI bus using DT host: cam=MmioCam ecam=0x10000 bus_end=0x0
[81839.156652 memspace::backend::linear:41] Linear::map: VA:0xffff800020033000..VA:0xffff800020043000 -> PA:0x10000..PA:0x20000 READ | WRITE | DEVICE
[81839.158086 kdriver::bus::pci:27] PCI config space: source=device-tree, ecam=0x10000, bus_end=0x0
[81839.159057 pci:214] PCI BAR allocator from device tree: cpu_base=0x2000000 size=0x2000000 prefetchable=false
[81839.160167 kdriver::bus::pci:46] PCI 00:00.0: 8086:1237 (class 06.00, rev 00) Standard
[81839.161244 kdriver::bus::pci:46] PCI 00:01.0: 1af4:1042 (class 01.80, rev 01) Standard
[81839.162220 pci:408]   BAR 0: MEM [0x2000000, 0x2008000)
[81839.163044 virtio:228] virtio PCI IRQ fallback: bdf=DeviceFunction { bus: 0, device: 1, function: 0 } hwirq=36 base=0x23
[81839.164389 memspace::backend::linear:41] Linear::map: VA:0xffff800020043000..VA:0xffff800020044000 -> PA:0x2000000..PA:0x2001000 READ | WRITE | DEVICE
[81839.165874 memspace::backend::linear:41] Linear::map: VA:0xffff800020044000..VA:0xffff800020045000 -> PA:0x2003000..PA:0x2004000 READ | WRITE | DEVICE
[81839.167378 memspace::backend::linear:41] Linear::map: VA:0xffff800020045000..VA:0xffff800020046000 -> PA:0x2001000..PA:0x2002000 READ | WRITE | DEVICE
[81839.168851 memspace::backend::linear:41] Linear::map: VA:0xffff800020046000..VA:0xffff800020047000 -> PA:0x2002000..PA:0x2003000 READ | WRITE | DEVICE
[81839.170307 virtio:161] PCI virtio device at DeviceFunction { bus: 0, device: 1, function: 0 }: IRQ = 4097
[81839.171447 kvirtiodrivers::transport:88] Device features: BlkFeature(SEG_MAX | BLK_SIZE | FLUSH | MQ | DISCARD | WRITE_ZEROES | RING_EVENT_IDX | VERSION_1 | ACCESS_PLATFORM)
[81839.173110 kvirtiodrivers::device::blk:64] found a block device of size 1048576KB
[81839.174626 kdriver::bus::pci:53] registered a new Block device at 00:01.0: "virtio-blk"
[81839.175630 kdriver::bus::pci:46] PCI 00:02.0: 1af4:1045 (class 08.80, rev 01) Standard
[81839.176556 pci:408]   BAR 0: MEM [0x2008000, 0x2010000)
[81839.177302 kdriver::bus::pci:46] PCI 00:03.0: 1b36:0011 (class ff.ff, rev 01) Standard
[81839.178213 pci:408]   BAR 0: MEM [0x2010000, 0x2010010)
[81839.179496 kdriver:140] number of NICs: 0
[81839.180002 kdriver:148] number of block devices: 1
[81839.180570 kdriver:151]   block device 0: "virtio-blk"
[81839.181196 kdriver:156] number of graphics devices: 0
[81839.181801 kdriver:164] number of input devices: 0
[81839.182376 kdriver:172] number of vsock devices: 0
[81839.182970 kfs:40] Initialize filesystem subsystem...
panicked at /home/jose/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/smallvec-1.15.1/src/lib.rs:1356:13:
assertion failed: index < len
Backtrace:
   0: __rustc::rust_begin_unwind
            at core/kruntime/src/lang_items.rs:11:21
   1: core::panicking::panic_fmt                         at library/core/src/panicking.rs:80:14
   2: core::panicking::panic                              at library/core/src/panicking.rs:150:5
   3: <smallvec::SmallVec<[...VirtIoBlkDev...; 1]>>::remove
            at smallvec-1.15.1/src/lib.rs:1356:13
   3: <kdriver::structs::DeviceContainer<...>>::take_nth
            at drivers/kdriver/src/structs/mod.rs:96:25
   3: kfs::init_filesystems
            at fs/kfs/src/lib.rs:48:18
   4: kruntime::rust_main
            at core/kruntime/src/lib.rs:245:9
   5: kernel_boot::arch::aarch64::entry::__primary_switched
            at boot/kernel-boot/src/lib.rs:22:13

[81839.207746 aarch64_peripherals::psci:98] Shutting down...
[2026-04-10T07:38:32 INFO  crosvm] vcpu requested shutdown
[2026-04-10T07:38:32 INFO  crosvm] exiting with success
```

### 9.3 与之前两轮 panic 的对比

| 阶段                        | 5.1 原始 pVM 尝试 | 6. 注释 `prepare_boot_memory` | 9. `prepare` no-op |
| --------------------------- | ----------------- | ----------------------------- | ------------------ |
| `memory space map ready`    | ✅                | ✅                            | ✅                 |
| LOGO + build info（UART）   | ❌                | ❌                            | ✅                 |
| `klogger::init_klogger`     | ❌                | ❌                            | ✅                 |
| GICv3 CPU interface         | ❌                | ❌                            | ✅                 |
| 调度器 / idle / gc          | ❌                | ❌                            | ✅                 |
| PCI 枚举 + BAR 分配         | ❌                | ❌                            | ✅                 |
| virtio-blk 1 GB 识别        | ❌                | ❌                            | ✅                 |
| `kfs::init_filesystems` 入口 | ❌                | ❌                            | ✅                 |
| Panic 位置                  | `psci.rs:45`      | `psci.rs:66`                  | `fs/kfs/src/lib.rs:48` + `kdriver/src/structs/mod.rs:96` |

也就是说 **`prepare` 短路后 boot 跑过了所有 pKVM / MMIO 相关路径**：UART
写 `0x3f8` 工作正常、GIC、PCI ECAM、virtio-blk 都工作正常。MMIO guard HVC
在 non-protected VM 下本来就是"不需要也不应该做"的那一步，我们之前的诊断
在这里得到了完全的确认。

## 10. 新的故障：`kfs::init_filesystems` 与 `take_nth` 的 off-by-one

这个 panic **和 pKVM 再没有任何关系**，是 kernel 侧的两个独立 bug
一起触发的。

### 10.1 Bug A：`kfs` 的 `crosvm` feature 期望两个 block device

`fs/kfs/src/lib.rs:39-55`:

```rust
pub fn init_filesystems(mut block_devs: DeviceContainer<KBlockDevice>) {
    info!("Initialize filesystem subsystem...");
    let dev = {
        #[cfg(feature = "crosvm")]
        {
            // must have two block devices: secure and non-secure
            // we only use the second blk
            block_devs
                .take_nth(1)
                .expect("Less than two block devices found!")
        }
        #[cfg(not(feature = "crosvm"))]
        {
            block_devs.take_one().expect("No block device found!")
        }
    };
    ...
}
```

这是一个 TEE 相关的约定：`crosvm` feature 打开时，平台假定会有两块磁盘
（index 0 = secure / TEE 专用，index 1 = non-secure / 内核真正用的 rootfs），
只取第二块。我们在 crosvm 启动命令里只传了一个 `--block`，结果容器里
只有 1 个设备。

### 10.2 Bug B：`DeviceContainer::take_nth` 的长度检查反了

`drivers/kdriver/src/structs/mod.rs:94-100`:

```rust
pub fn take_nth(&mut self, n: usize) -> Option<D> {
    if self.len() >= n {          // ← BUG: 应当是 `n < self.len()` 或 `self.len() > n`
        Some(self.0.remove(n))
    } else {
        None
    }
}
```

当 `len=1, n=1` 时，`len >= n` 成立，于是去调 `SmallVec::remove(1)`，
smallvec 内部断言 `index < len` 失败，于是就有了现在这条 panic 栈。

正确的写法应当是：

```rust
pub fn take_nth(&mut self, n: usize) -> Option<D> {
    if n < self.len() {
        Some(self.0.remove(n))
    } else {
        None
    }
}
```

即便修了这个 off-by-one，`take_nth(1)` 在 `len=1` 时仍然会返回 `None`，
紧接着的 `.expect("Less than two block devices found!")` 还是会 panic，
只是**错误信息会更清楚**。要真正通过这一步，必须在"设备数量"那一侧
让 Bug A 的前提成立。

### 10.3 三条可选前进路线

1. **路 1 —— 多传一个 `--block`**：最贴近 `crosvm` feature 的原设计意
   图，不碰任何 kernel 代码。需要在设备上先造一个 dummy 小 img（AVF
   crosvm 大概率不允许同一个文件挂两次），然后启动命令变成：

   ```bash
   crosvm run ... \
       --block /data/local/tmp/jose/dummy.img \   # index 0（"secure"）
       --block /data/local/tmp/jose/disk.img \    # index 1（"non-secure"，真 ext4）
       ...
   ```

2. **路 2 —— 修两个 kernel bug**：把 `take_nth` 的 `>= n` 改成 `> n`
   （或者 `n < self.len()`），再把 `kfs::init_filesystems` 的 `crosvm`
   分支改成"先试 `take_nth(1)`、拿不到就回退到 `take_one()` / `take_nth(0)`"。
   这条路改动超出"绕过 pKVM"的范畴，需要和维护者商量是否接受。

3. **路 3 —— 只修 off-by-one + 启动多带一个 dummy block**：综合路 1 和
   路 2，风险最小：一个明显的小 bug 顺手修掉，kernel 侧"crosvm 必须两
   盘"的设计约束留给后续讨论。

目前倾向路 3（下一轮），因为它同时：

- 留下一个真实的 kernel bug 修复作为副产品；
- 完全遵守 `crosvm` feature 的原设计意图；
- 不需要把 `kfs::init_filesystems` 的 TEE 相关逻辑动起来。

## 11. 第二轮修复：`take_nth` + `kfs::init_filesystems` 单盘回退

最终选择的是 **"路 2"** 的变体：两处小改动，不动启动命令。

### 11.1 Bug B 的最小修复：`drivers/kdriver/src/structs/mod.rs`

```diff
 pub fn take_nth(&mut self, n: usize) -> Option<D> {
-    if self.len() >= n {
+    if n < self.len() {
         Some(self.0.remove(n))
     } else {
         None
     }
 }
```

### 11.2 Bug A 的最小修复：`fs/kfs/src/lib.rs`

保留 crosvm feature 下"优先用第二块盘"的原设计意图，只加一个"单盘回退"：

```diff
     let dev = {
         #[cfg(feature = "crosvm")]
         {
-            // must have two block devices: secure and non-secure
-            // we only use the second blk
+            // Prefer the second blk (non-secure rootfs when secure+non-secure
+            // are both present); fall back to the only blk if there is just one.
             block_devs
                 .take_nth(1)
-                .expect("Less than two block devices found!")
+                .or_else(|| block_devs.take_nth(0))
+                .expect("No block device found!")
         }
         #[cfg(not(feature = "crosvm"))]
         {
             block_devs.take_one().expect("No block device found!")
         }
     };
```

## 12. 第二轮运行（`--cpus 1`）—— ext4 已经挂上来了！

### 12.1 启动命令（同前，一个 `--block`）

```bash
adb shell 'su 0 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox --protected-vm-without-firmware \
    --cpus 1 --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

### 12.2 关键新增输出

`kfs::init_filesystems` 顺利回退到 `take_nth(0)`，然后 rsext4 驱动**成功挂载**
了 1 GB 的 ext4 rootfs：

```
[kfs:40] Initialize filesystem subsystem...
[kfs:57]   use block device 0: "virtio-blk"
[rsext4::ext4:206] Start mounting Ext4 filesystem...
[rsext4::ext4:222] Superblock magic verified
[rsext4::ext4:232] Block group count: 8
[rsext4::ext4:465] Loading group descriptors: 8 groups, desc_size = 64 bytes
[rsext4::ext4:497] Successfully loaded 8 group descriptors
[rsext4::ext4:256] Inode cache initialized
[rsext4::ext4:260] Data block cache initialized
...(s_last_mounted = "/home/yean/code/kylin-rootfs/mnt")...
[rsext4::ext4:280] Checking root directory...
[rsext4::ext4:189] Root inode i_mode: 16877
[rsext4::ext4:296] Checking lost+found directory...
[rsext4::ext4:158] Found it: /lost+found !
[rsext4::ext4:311] /lost+found exists (path resolution)
[rsext4::ext4:426] Bitmap usage: inodes used = 25089, data blocks used = 4345
[rsext4::ext4:433] Ext4 filesystem mounted
[rsext4::ext4:434]   - block size: 4096 bytes
[rsext4::ext4:435]   - total blocks: 262144
[rsext4::ext4:436]   - free blocks: 246902
[rsext4::ext4:437]   - total inodes: 65536
[rsext4::ext4:438]   - free inodes: 65007
[kfs:60]   filesystem type: "ext4"
[knet:68] Initialize network subsystem...
[knet:108]   No network device found!
[knet:113] Device: lo
[knet:143] Initialize vsock subsystem...
[knet:150]   No vsock device found!
[fbdevice:20] Initialize framebuffer subsystem...
[inputdev:23] Initialize input subsystem...
[kruntime::mp:28] starting CPU 1...
[aarch64_peripherals::psci:107] Starting CPU 1 ON ...
[aarch64_peripherals::psci:110] failed to boot CPU 1 (InvalidParams)
```

### 12.3 卡点：`--cpus 1` 下 SMP 等不到第二颗 CPU

输出到 `failed to boot CPU 1 (InvalidParams)` 之后**就没有任何后续日志**，
VM 卡住直到 60 秒超时。这是因为：

- defconfig 里 `CPU_NUM=2`（编译期常量）
- crosvm 命令行给了 `--cpus 1`，guest 只有 1 个 vCPU
- Primary CPU 跑到 `kruntime::mp::start_secondary` 向 PSCI 发 `CPU_ON`，
  pKVM 返回 `InvalidParams`，然后 kernel 继续走，但后续 init 里有某个
  同步点在等 `CPU_NUM` 个 CPU 全部上线（或者 `kserveices::init` 开了
  进 `info!` 级日志前 log 路径被 hang），所以看不到更多输出

## 13. 第三轮运行（`--cpus 2`）—— VFS/devfs 组装，卡在 DICE

### 13.1 启动命令

```bash
adb shell 'su 0 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox --protected-vm-without-firmware \
    --cpus 2 --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

### 13.2 全新的日志（`--cpus 2` 解锁了一大片）

```
[kruntime:313]   [PA:0x2000000, PA:0x2001000) [VA:0xffff800020063000, VA:0xffff800020064000) virtio-mmio-hal
...(4 个 virtio-mmio-hal 映射)...
[kruntime:274] Primary CPU 0 init OK.                          ← 这一行是 --cpus 1 时永远到不了的
[0:2 kserveices:36] Initialize VFS...
[1:5 ktask::run_queue:423] task block: Task(5, "gc")           ← CPU 1 上有 gc 任务，说明 SMP 活了
[0:2 kserveices::vfs::dev::dtb:29] Captured DTB snapshot: paddr=0xffe00000 size=0x200000
[0:3 ktask::run_queue:423] task block: Task(3, "gc")
[0:2 ktask::task:170] new task: Task(6, "tty-reader")
[0:2 ktask::run_queue:240] task add: Task(6, "tty-reader") on run_queue 0
panicked at core/kservices/src/vfs/dev/dice.rs:42:18:
called `Option::unwrap()` on a `None` value
Backtrace:
   3: core::option::unwrap_failed
   4: <kserveices::vfs::dev::dice::DiceNodeInfo>::new
            at core/kservices/src/vfs/dev/dice.rs:42:18
   4: kserveices::vfs::dev::builder
            at core/kservices/src/vfs/dev/mod.rs:447:22
   5: <kcore::vfs::fs::SimpleFs>::new_with_flags::<builder>
            at core/kcore/src/vfs/fs.rs:75:20
   6: kserveices::vfs::dev::new_devfs
            at core/kservices/src/vfs/dev/mod.rs:40:5
   6: kserveices::vfs::mount_all
            at core/kservices/src/vfs/mod.rs:58:27
   7: kserveices::init
            at core/kservices/src/lib.rs:38:5
   8: main
            at entry/src/main.rs:45:5
   9: kruntime::rust_main
            at core/kruntime/src/lib.rs:281:14
```

### 13.3 故障分析：DICE 节点在非 protected-with-firmware 下没有数据

`core/kservices/src/vfs/dev/dice.rs:37-45`:

```rust
impl DiceNodeInfo<'static> {
    pub fn new() -> Self {
        DiceNodeInfo {
            _compatible: DICE_COMPATIBLE,
            regions: dice_region()
                .map(|reg| (khal::mem::p2v(pa!(reg.starting_address as usize)), reg.size))
                .unwrap(),                                                  // ← 42:18
            _no_map: false,
        }
    }
}
```

调用链：`kserveices::init → vfs::mount_all → vfs::dev::new_devfs →
vfs::dev::builder → DiceNodeInfo::new`。`builder` 里 `#[cfg(all(feature =
"dice", target_os = "none"))]` 分支无条件创建 `/dev/dice` 节点
（`core/kservices/src/vfs/dev/mod.rs:441-449`）：

```rust
#[cfg(all(feature = "dice", target_os = "none"))]
root.add(
    "dice",
    Device::new(
        fs.clone(),
        NodeType::CharacterDevice,
        DeviceId::new(30, 0),
        Arc::new(dice::DiceNodeInfo::new()),
    ),
);
```

但 `dice_region()` 是从 DTB 的 `/reserved-memory` 里找 `compatible =
"kylin,open-dice"` 或 `google,open-dice` 节点。Android AVF 下，这个节点
**是由 pvmfw 在 protected boot 时写进 guest 物理内存 + DTB 的**，我们
用 `--protected-vm-without-firmware` 显式跳过了 pvmfw，自然也就没有
DICE 区域。于是 `dice_region().unwrap()` 直接爆。

### 13.4 三轮 panic 全景对比

| 阶段                       | §9 `prepare` no-op | §12 `--cpus 1` + kfs fix | §13 `--cpus 2` + kfs fix |
| -------------------------- | ------------------ | ------------------------ | ------------------------ |
| `memory space map ready`   | ✅                 | ✅                       | ✅                       |
| LOGO + klogger             | ✅                 | ✅                       | ✅                       |
| GICv3 / 调度器             | ✅                 | ✅                       | ✅                       |
| PCI + virtio-blk 识别      | ✅                 | ✅                       | ✅                       |
| **ext4 挂载**              | ❌                 | ✅                       | ✅                       |
| network / vsock / fb / input | ❌               | ✅                       | ✅                       |
| SMP（CPU 1 上线）          | ❌                 | ❌                       | ✅                       |
| `Primary CPU 0 init OK`    | ❌                 | ❌                       | ✅                       |
| `kserveices::init` → VFS mount | ❌             | ❌                       | ✅                       |
| `devfs builder`            | ❌                 | ❌                       | ✅ 进入                   |
| `/dev/dice` 构造           | —                  | —                        | ❌ panic                  |
| Panic 位置                 | `kfs:48`           | （hang，非 panic）        | `dice.rs:42`             |

## 14. 第四轮尝试：用 `custom_pvmfw` 走 protected-with-firmware

受 §13 DICE 缺失的启发，设备上 `/data/local/tmp/custom_pvmfw`
（约 916 KB）本来就是为这套环境准备的 pvmfw，理论上它会：

1. 在 guest 物理内存里布置好 DICE chain；
2. 给 DTB 加 `/reserved-memory/dice` 节点；
3. 然后 chain-load 我们的 `kernel.bin`。

这样 `dice_region()` 就会返回 `Some(...)`，`dice.rs:42` 的 `unwrap()` 也就
不会炸。

### 14.1 启动命令

```bash
adb shell 'su 0 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox \
    --protected-vm-with-firmware /data/local/tmp/custom_pvmfw \
    --cpus 2 --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

### 14.2 输出：pvmfw 拒绝了 DT 里的内存 base

```
[2026-04-10T07:48:58 DEBUG crosvm] creating hypervisor: Kvm
[2026-04-10T07:48:58 INFO  crosvm] Trying to attach block device: /data/local/tmp/jose/disk.img
[2026-04-10T07:48:58 INFO  disk] disk size 1073741824
[INFO] pvmfw config version: 1.1
[ERROR] Failed to use memory range value from DT: 0x7fc00000..0x100000000: Received different base address
[2026-04-10T07:48:58 INFO  crosvm] vcpu requested reset
[2026-04-10T07:48:58 INFO  crosvm] exiting with success
```

### 14.3 扫了几个常见 `--mem` 大小，全部被拒

| `--mem` | DT 里的 memory range         | pvmfw 报错                        |
| ------- | ---------------------------- | --------------------------------- |
| 2048    | `0x7fc00000..0x100000000`   | Received different base address  |
| 1024    | `0x7fc00000..0xc0000000`    | Received different base address  |
|  512    | `0x7fc00000..0xa0000000`    | Received different base address  |
|  256    | `0x7fc00000..0x90000000`    | Received different base address  |

规律：无论 `--mem` 多大，crosvm 都把 guest RAM base 固定在
`0x7fc00000`（比标准的 AVF base `0x8000_0000` 低 4 MB）。这 4 MB
本来就是 crosvm 留给 pvmfw 自己驻留/加载用的空间，但 `custom_pvmfw`
构建时嵌入的 config 里期望的是另一个 base——两边对不上。

### 14.4 可能的走法

1. **改 pvmfw**：重新构建 `custom_pvmfw`，让它接受 `0x7fc00000` 这个
   base（或者让它从 DT 动态读）。需要 pvmfw 源码 / 构建环境。
2. **改 crosvm 参数**：看 AVF crosvm 有没有类似 `--pci-ram-base` /
   `--ram-low` 之类的开关允许调整 guest RAM base。目前 `run --help`
   里没看到明显的选项。
3. **继续走 `--protected-vm-without-firmware`**：把 kernel 端 §13
   的 DICE panic 也 guard 掉（`dice_region()` 拿不到就不创建
   `/dev/dice`），这是最小改动路线，下一步就可以做。

## 15. 现在的整体进度

- ✅ pKVM MMIO guard 路径绕开（§8–§9）
- ✅ `take_nth` off-by-one 修掉（§11.1）
- ✅ `kfs::init_filesystems` 单盘回退（§11.2）
- ✅ 1 GB ext4 rootfs 已经被 rsext4 成功挂载（§12.2）
- ✅ SMP 两核上来（§13.2）
- ✅ VFS / devfs 组装过程进到 `DiceNodeInfo::new`（§13.2）
- ⚠️ 两条下一步互斥但都可行的路：
  - **A**: 在 `dice.rs` 里把 `unwrap()` 换成 `dice_region().map(...)`
    然后在 `builder` 里按可得性再决定要不要 `/dev/dice`。几分钟就能做，
    可以直接跑通到 userland。
  - **B**: 在 `custom_pvmfw` / crosvm 参数里把 RAM base 对齐到
    `0x8000_0000`。"更正经"但需要额外的构建/调研。

## 16. 第五轮：跳过 `/dev/dice` → **shell 起来了**

选了 §15 的**路 A**：给 DICE 节点加一个运行时守卫，没有 `dice_region()`
就不建 `/dev/dice`。

### 16.1 改动

#### 16.1.1 `core/kservices/src/vfs/dev/dice.rs` 新增 helper

```diff
 const DICE_COMPATIBLE: &str = "kylin,open-dice";
 const MAX_DICE_DATA_SIZE: usize = 0x1000; // 4KB

+/// Whether the DICE handover region is described in the device tree.
+///
+/// On AVF protected VMs the pvmfw writes the DICE chain into guest memory
+/// and adds a `/reserved-memory` node for it; without pvmfw (e.g. a
+/// `--protected-vm-without-firmware` run) that node is absent and
+/// `DiceNodeInfo::new()` would panic. Callers should gate `/dev/dice`
+/// construction on this check.
+pub fn available() -> bool {
+    dice_region().is_some()
+}
+
 impl DiceNodeInfo<'static> {
     pub fn new() -> Self {
```

#### 16.1.2 `core/kservices/src/vfs/dev/mod.rs` 在 builder 里守卫

```diff
+    // /dev/dice is only meaningful when pvmfw (or similar) has published a
+    // DICE handover region in the device tree's /reserved-memory. Without it
+    // (e.g. a --protected-vm-without-firmware run), skip the node entirely
+    // instead of panicking inside DiceNodeInfo::new.
     #[cfg(all(feature = "dice", target_os = "none"))]
-    root.add(
-        "dice",
-        Device::new(
-            fs.clone(),
-            NodeType::CharacterDevice,
-            DeviceId::new(30, 0),
-            Arc::new(dice::DiceNodeInfo::new()),
-        ),
-    );
+    if dice::available() {
+        root.add(
+            "dice",
+            Device::new(
+                fs.clone(),
+                NodeType::CharacterDevice,
+                DeviceId::new(30, 0),
+                Arc::new(dice::DiceNodeInfo::new()),
+            ),
+        );
+    }
```

#### 16.1.3 编译期踩到的坑：`mod log` 和 `log` crate 撞名

第一版改动里我在 else 分支加了 `log::warn!(...)`，结果编译报错：

```
error[E0433]: failed to resolve: could not find `warn` in `log`
  --> core/kservices/src/vfs/dev/mod.rs:452:14
```

原因是这个文件里有一行 `#[cfg(feature = "dev-log")] mod log;`——这是
实现 `/dev/log` 的本地子模块，**名字正好盖住了 `log` crate**。而且
`kservices` 的 `Cargo.toml` 根本不依赖 `log` crate，所以连 `::log::warn!`
这种全路径也不行（"could not find `log` in the list of imported crates"）。
最简单的处理是把 warning 删掉，把"为什么有这个 guard" 用代码注释解释清楚：

```rust
// /dev/dice is only meaningful when pvmfw (or similar) has published a
// DICE handover region in the device tree's /reserved-memory. Without it
// (e.g. a --protected-vm-without-firmware run), skip the node entirely
// instead of panicking inside DiceNodeInfo::new.
```

### 16.2 运行结果：715 行日志，shell 真的起来了

启动命令（同 §13.1）：

```bash
adb shell 'su 0 /apex/com.android.virt/bin/crosvm --log-level debug run \
    --disable-sandbox --protected-vm-without-firmware \
    --cpus 2 --mem 2048 \
    --block /data/local/tmp/jose/disk.img \
    /data/local/tmp/jose/kernel.bin'
```

日志一共 715 行，从 `[boot]` 一直跑到 **busybox 交互式 shell 的 prompt**。
几个关键里程碑（行号来自 `doc/pvm_dice_guard.log`）：

| 行号 | 事件                                                                 |
| ---- | -------------------------------------------------------------------- |
| 53   | `Primary CPU 0 started`                                             |
| 173  | `Primary CPU 0 init OK`                                             |
| 174  | `kserveices: Initialize VFS...`                                      |
| 376–384 | Task 10 (`busybox`) 写出 `SHLVL=1` / `HOME=/root` / `PWD=/`（一个 `env` 命令） |
| 388  | `Task(10, "busybox") exit with code: 0`                             |
| 389  | `Send signal SIGCHLD to process 9`                                  |
| 439  | `new task: Task(11, "busybox")`                                     |
| 454  | `task add: Task(11, "busybox") on run_queue 1`                      |
| 706  | `sys_writev <= fd: 2, iovcnt: 1`                                    |
| 707  | `kylin-x:~# ` ← **shell prompt 真的被打出来了**                        |
| 710–711 | Task 11 又 `writev(fd=2, 4B)` 发出 `\x1b[6n`（ANSI 光标位置查询） |
| 715  | `task block: Task(11, "busybox")` ← shell 在 `ppoll(stdin)` 上阻塞等键盘 |

Shell prompt 直接出现在 log 流里的这一行是最令人信服的证据：

```
kylin-x:~# [83642.721542 1:11 ksyscall::dispatch:666] Syscall writev return Ok(11)
```

`Ok(11)` 正好对应 `"kylin-x:~# "` 11 个字节。

### 16.3 整个用户态启动路径也全部都在

从日志还能看到 `kruntime::mp` SMP 初始化、大量 `mprotect` / `mmap` 构造
进程地址空间、`/proc` / `/dev/tty` 相关 ioctl、`SIGWINCH`/`SIGCHLD`
信号路径、**JBD2 journal commit**（tid=6/7/8，group descriptor
writeback）——也就是说 ext4 不光被挂上了，还是**读写模式**在工作，
journal 正在正常提交。

### 16.4 为什么 shell "看起来卡住"

最后 60 秒 `adb shell timeout 60 crosvm run ...` 会被 kill，但内核本身
是活的：`Task(11, "busybox")` 正在 `ppoll([fd 0], None)`——也就是**无限
等待 stdin 可读**。我们这种"`adb shell` 包一层 `crosvm` 包一层 kernel"
的管道没有把任何字符喂到 guest 的 serial 输入，所以 shell 确实永远
看不到一个 byte。

要真正能进去打字，有几条路（都属于"配置问题"，和 kernel 本身无关）：

1. **`adb exec-out` + crosvm `--serial type=stdin,...`**（把 stdin
   显式转发给 VM）；
2. **TCP/pty 反向**：在 Android 上起一个 socat / busybox nc 把
   crosvm serial 的 `type=unix-stream` 暴露出来，host 上 `adb forward` 接过来；
3. **最简单的冒烟验证**：直接在启动命令前 `echo -e 'uname -a\npoweroff\n' |`
   pipe 进 crosvm，让它自己消化一条命令然后退出。

这些都和 x-kernel / pKVM / pvmfw 不再有关系。

## 17. 当前进度总结（更新）

- ✅ pKVM MMIO guard 路径绕开（§8–§9）
- ✅ `take_nth` off-by-one 修掉（§11.1）
- ✅ `kfs::init_filesystems` 单盘回退（§11.2）
- ✅ `--cpus 2` + CPU_NUM 对齐（§13）
- ✅ `/dev/dice` 运行时守卫（§16.1）
- ✅ **1 GB ext4 已挂载，JBD2 journal 在 commit**
- ✅ **busybox shell 已经启动，并在等 stdin**
- ⬜ 把 stdio 转进 VM，让 shell 能实际交互（纯用户空间/配置问题）
- ⬜ （可选）走 `--protected-vm-with-firmware` 需要重新构建一个接受
  RAM base `0x7fc00000` 的 `custom_pvmfw`；目前跑的路线 `--protected-vm-without-firmware`
  已经能完整到 userland，pvmfw 路径不是阻塞项

## 18. 真 pvmfw 机器上的复现 —— stage-2 external abort

为了在**另一台原生支持 pvmfw 的 pKVM 机器**上验证这个 image，我们把
`core/kservices/src/vfs/dev/dice.rs` 和 `core/kservices/src/vfs/dev/mod.rs`
的运行时 DICE guard 回退掉（那台机器的 pvmfw 会正常发布 DICE
region，用不到 guard），**保留** §8–§9 的 MMIO guard 绕过（`init.rs`
注释 + `psci.rs` no-op）以及 §11 的两个 bug 修复。推过去重跑，日志
立刻在 `memory space map ready` 之后爆掉：

```
[boot] firmware: chosen stdout-path=/uart@3f8
[boot] firmware: chosen bootargs=panic=-1
[boot] firmware: using chosen bootargs as cmdline=panic=-1
[boot] kruntime memory regions ready
[boot] kruntime allocator ready
[boot] memory space map ready
[2026-04-10T08:44:40.933387349+00:00 ERROR devices::serial] Failed to create wait context. Operation not permitted (os error 1)
panicked at arch/kcpu/src/aarch64/excp.rs:117:21:
Unhandled synchronous exception Some(DataAbortCurrentEL) @ 0xffff80000017068c: ESR=0x96000010 (EC 0b100101, FAR: VA:0xffff800020002000 ISS 0x10)
Backtrace:
Backtracing is not initialized. Raw frames:
     0: fp=0xffff800003710e60, ip=0xffff80000017068d
...
[2026-04-10T08:44:40.935145936+00:00 INFO  crosvm::crosvm::sys::linux::vcpu] system shutdown event on vcpu 0
[2026-04-10T08:44:40.935212097+00:00 INFO  crosvm::crosvm::sys::linux] vcpu requested shutdown
[2026-04-10T08:44:41.080416705+00:00 INFO  crosvm] exiting with success
```

### 18.1 ESR 解码

`ESR = 0x96000010`：

| 字段 | 取值 | 含义 |
|------|------|------|
| EC `[31:26]` | `0b100101` (0x25) | Data Abort taken from current EL (EL1→EL1) |
| IL `[25]`    | `1`               | 32-bit instruction |
| ISV `[24]`   | `0`               | Instruction syndrome not valid |
| ISS `[23:0]` | `0x000010`        | ← 见下面展开 |

ISS `0x10 = 0b0001_0000` 展开：

| 位 | 取值 | 含义 |
|----|------|------|
| WnR `[6]`      | `0`           | **Read** fault |
| DFSC `[5:0]`   | `0b010000` (0x10) | **Synchronous External abort, not on translation table walk or hardware update of translation table** |

也就是说 **stage-1 的 VA→PA 映射是存在的**（DFSC 不是 translation fault
或 access flag fault），**物理访问真的到了总线这一层**，然后 pKVM 的
stage-2 / 外设子系统拒绝了这次读——这是一次"外部中止"(external abort)。

`FAR = 0xffff800020002000` 落在 x-kernel 的 iomap 窗口里（比较一下 §9
里 `memspace::backend::linear::Linear::map` 打印的那些目的 VA，都是
`0xffff80002xxx_xxxx`），所以就是某个 `iomap_device` 建好的 MMIO 页在被
某个 early driver 第一次读的时候炸掉。

故障 IP `0xffff80000017068c`，栈 fp `0xffff800003710e60`——由于
`Backtracing is not initialized` 没法直接还原调用点，但从"`memory
space map ready` 已打印、`LOGO / klogger` 还没出来"这个时间窗口看，一定
发生在 `core/kruntime/src/lib.rs:172 khal::early_driver_init();`
里第一次碰到 MMIO 的地方——大概率是 `console_driver::init_from_device_tree()`
读 UART IIR/LSR 之类的寄存器。

### 18.2 根本原因：我们的 `prepare` no-op 把 MMIO guard HVC 干掉了

§8 之所以能让"测试机"跑起来，是因为那台 10.88.21.42 上的 pKVM 实现对
MMIO stage-2 比较宽松：即使我们没有发任何 `KVM_MMIO_GUARD_MAP` HVC，
guest 访问 MMIO 页仍然能打到真实设备。所以我们以为可以把
`PlatformMmioIf::prepare` 彻底短路掉。

**但在原生支持 pvmfw 的 pKVM 实现上，这条假设不成立**。pvmfw 规范下的
protected VM 有严格的 stage-2：

- pKVM 启动时不会给 guest 预装任何 MMIO 的 stage-2 映射；
- guest 必须通过 `ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_MAP` HVC
  (`psci.rs:37 __invoke_mmioguard` 里那条) 逐页申请：
  "请把物理地址 X 的这一页放进我的 stage-2"；
- 没有被 HVC 申请过的页，访问时就会触发 external abort（DFSC=0x10）——
  **和我们现在看到的一模一样**。

也就是说，**`PlatformMmioIf::prepare` 里的 `do_xmap_granules` 不是装饰
性代码，而是 guest 与 pKVM 之间关于 "我想看见这个 MMIO" 的唯一约定**。
把它 no-op 掉等于承诺"我永远不会碰任何 MMIO"，然后第一行驱动代码就
会背叛这个承诺。

### 18.3 另一条红色日志：`devices::serial` EPERM，和 panic 无关

```
[ERROR devices::serial] Failed to create wait context. Operation not permitted (os error 1)
```

这是 **host 侧 crosvm 自己**打出来的，发生在 guest 还没真正跑之前：
crosvm 的 serial 后端在 setup 时想建一个 `WaitContext`（底层是
`epoll_create1` 之类），被 seccomp/minijail 或命名空间权限挡回来了，返回
`EPERM`。后果是 serial **input** 不会被挂进 poll 循环——guest 永远收
不到键盘字符——但 serial **output** 还是正常工作（所以我们能看到这些
`[boot]` 日志）。

几个可能的起因：

1. 我们加了 `--disable-sandbox`，但这台真机的 AVF crosvm 构建强制
   minijail，把某些 syscall 从默认允许列表里拿掉了；
2. 这台设备的 SELinux / appdomain 不允许 `shell` uid 用 epoll
   on some fd type；
3. crosvm 自己在 protected 模式下收紧了 serial 的 wait context 策略。

**和 panic 无关**——real fix 是排查真机上的 sandbox/selinux policy，或
者看能不能给 crosvm 用 `--serial type=stdout,hardware=serial,...` 把
in/out 拆开。可以等 data abort 解决之后再回头看。

### 18.4 处理方向（等将来回来做的时候）

1. **最小回归**：把 `platforms/aarch64-crosvm-virt/src/init.rs` 和
   `platforms/aarch64-crosvm-virt/src/psci.rs` `git checkout --` 回
   HEAD，恢复 `kvm_guard_granule_init()` 和 `PlatformMmioIf::prepare →
   do_xmap_granules` 的完整调用链。只保留 §11 的 `take_nth` 和 `kfs`
   两个真 bug 修复。这样在 pvmfw 真机上 HVC 应该正常返回，stage-2 能
   被正确填充。
2. **兼容两套环境**：理想的长期方案是在 `aarch64-crosvm-virt` 的
   `Cargo.toml` 里加一个 feature（比如 `soft-pkvm` 或 `no-mmio-guard`），
   让测试机那种宽松 pKVM 环境编译时选择短路，真机编译时走完整 HVC
   路径。两种二进制共存，不需要每次手动改源码。
3. **运行时自检（更稳的方案）**：`kvm_guard_granule_init()` 里那条
   `MMIO_GUARD_INFO` HVC 本来就返回 `guard_has_range`——可以把它从
   `assert_eq!(guard_has_range, 0x1)` 改成"是 1 就启用 guard，不是 1
   就把 `PlatformMmioIf::prepare` 设成 no-op"，然后用一个 `Once<bool>`
   记住。这样同一个二进制同时适配两种环境，代价是所有 iomap 多走一次
   原子分支。
4. **别忘了 `devices::serial` 的 EPERM 噪音**：和 panic 独立，但会影响
   userland 里 shell 是否能交互。等 data abort 解决之后再看。

回到这个状态只需要一条命令：

```bash
git checkout -- platforms/aarch64-crosvm-virt/src/init.rs \
                 platforms/aarch64-crosvm-virt/src/psci.rs
```

之后 `make build` 出来的 image 就是"面向真 pvmfw pKVM 的版本"，只带
§11 的两个小 bug 修。
