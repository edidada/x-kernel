# drv-clk — 设计文档

## 定位

driver-base 模型下的**时钟 provider 驱动**聚合 crate。当前只有一个驱动：
`fixed_clock`，认领 DT 的 `compatible = "fixed-clock"` 节点，把一个固定频率
输出发布到 `kclk` 的 provider registry。

它是**provider 驱动的 golden 模板**——与 `drv/serial/pl011`（消费者模板）、
`drv/virtio/blk`（PCI + 类发布模板）、`drv/block/ramdisk`（最简模板）并列。
provider 驱动的形状与消费者不同：它不获取资源，它**发布**一个被别人依赖的
能力，并因此唤醒 parked 的消费者。

## 范围

| 文件 | 内容 |
|---|---|
| `src/lib.rs` | crate 门面（`#[cfg(feature = "fixed-clock")] pub mod fixed_clock;`） |
| `src/fixed_clock.rs` | `FixedClockDriver` + `FixedClockData` + `FixedClockLookup` + `IDS` + `register()` |

## 架构

```text
DT 节点                              driver-init::build_descriptor
  clk24m: clock-24m {                    └─ FirmwareNodeInfo {
    compatible = "fixed-clock";                phandle: Some(0x…)
    #clock-cells = <0>;                        clock_frequency: Some(24000000)
    clock-frequency = <24000000>;              clock_cells: Some(0)
  };                                           clock_output_name: Option<&str>
                                           }
        │ add_device → probe
        ▼
FixedClockDriver::probe(dev)
   ├─ 读 dev.descriptor().firmware 的 phandle / clock_frequency / clock_output_name
   ├─ ClkBuilder::new(name, FixedRate::new(freq).into_ops()).build()  → Arc<ClkCore>
   ├─ Box::leak(Box::new(FixedClockLookup { provider_id: phandle, core }))
   └─ dev.env().provide::<driver_host::ClkProvider>(phandle, lookup)?   // ← 通用 provider 门
        │
        ▼
   driver_host::Host::register_provider（ProviderServices）
        ├─ 校验 key 与 lookup.provider_id() 一致（不一致 → Fatal(Invalid)）
        ├─ driver_host::ClkRegistry::register_provider（重复 phandle → Fatal(AlreadyExists)）
        │     ├─ kclk::register_provider(phandle, lookup)
        │     └─ wake_hook(DepSource::clk(phandle))
        │          └─ driver_base::global_domain().wake(dep)
                  └─ 所有 parked 在这个 phandle 上的消费者被精确重探
```

消费者侧（例如 pl011）只写 `dev.res().clk("baudclk")`：provider 未注册时
`ClkRegistry::get` 把 `kclk::ClkError::NoProvider` 翻译成
`ServiceError::Deferred(DepSource::clk(phandle))`，域 park 设备。**没有轮询、
没有重试表、没有顺序表。**

## 调用约束 / 执行上下文

- `probe` 由域在锁外的任务上下文调用，可睡眠（`ClkBuilder::build` 分配）。
- `dev.env().provide::<ClkProvider>` 走的是本设备的 `DeviceServices`（`Host` 实例），
  不访问进程级全局，因此没有"未安装 runtime 就 panic"的路径；注册失败以
  `ProbeError::Fatal` 返回（key 不一致 / 重复 phandle / host 不支持该家族）。
- `register()` 必须在**消费者驱动之前**调用吗？**不必须**。deferred probe 的
  全部意义就是顺序无关：先 probe 的消费者会 defer，provider 发布时被唤醒。
  compose 里 `fixed_clock` 排在第一位只是为了减少一轮 defer。
- 本 crate 无锁、无原子、无中断处理、无 MMIO。

## 设计决策

### `Box::leak` 是刻意的，不是泄漏 bug

```rust
let lookup: &'static FixedClockLookup = Box::leak(Box::new(FixedClockLookup { … }));
dev.env().provide::<driver_host::ClkProvider>(phandle, lookup)?;
```

`ProviderClass::Lookup = dyn kclk::ClkLookup`，所以这条调用要求
`kclk` 的 provider registry 是 **boot-static** 的（时钟控制器与内核同生命周期，
对应 Linux `of_clk_add_hw_provider` 同样不注销）。`FixedClockData` 另外持有
`Arc<ClkCore>`，所以时钟树本身由 `Data` 拥有；泄漏的只是那个 8 字节的 lookup
适配器。

`remove()` 是空的，并且注释写明"Clocks are boot-static; the lookup intentionally
outlives the device"。**这是一个刻意的语义选择**：移除一个 fixed-clock 设备不会
撤销 provider。若将来需要可注销的 provider，`kclk` 的 registry 要先支持注销
（`kclk::Registry::unregister` 已存在但当前无调用者，构建时报 dead_code）。

### `get_by_name` 返回 `None`

`#clock-cells = <0>` 的节点没有 SoC 输出名，只有 index 0。所以
`get_by_index(0)` 返回时钟，`get_by_name(_)` 一律 `None`。消费者用
`dev.res().clk("baudclk")` 时走的是 DT `clock-names` → `ClkSpec::by_phandle_index`
的 index 路径，不是 name 路径。

### 名字来自 `clock-output-names`，回退到 `"fixed-clock"`

`firmware.clock_output_name.unwrap_or("fixed-clock")` 是 `&'static str`
（借自 DT blob），因此 `ClkBuilder::new(name, …)` 不需要分配。

### 依赖 `driver_host` 与 `kclk`（登记的例外 E-02c）

provider 驱动的职责**就是**往主机时钟子系统注册，而注册的**形状**由通用的
`ProviderClass` 约定给出：本驱动只命名家族描述符
（`driver_host::ClkProvider`），不调用任何注册函数、不自己唤醒消费者。
`FixedClockLookup` 要构造 `kclk::ClkCore`，时钟树的类型本来就属于 `kclk`，
所以 `driver_host`/`kclk` 依赖仍然登记在 E-02c。
见 `docs/ai/driver-subsystem-boundaries.md` 例外 E-02。

## 第二个驱动：`fixed_factor`（转写验证用）

`src/fixed_factor.rs` 是 Linux `drivers/clk/clk-fixed-factor.c` 的转写，形态是
**一个 probe 里同时是消费者和 provider**：

```text
dev.res().clk("parent")             // 固件引用 clocks = <&apb_pclk 0>
  → rate = parent_rate * mult / div
dev.env().provide::<ClkProvider>(PROVIDER_ID, lookup)?   // 最后一步
```

QEMU `virt` 只有一个时钟节点（`apb-pclk`, phandle `0x8000`，已被 `fixed_clock`
认领），没有派生时钟的 DT 节点，所以这个设备由组合根作为**板级静态设备**
（alias `clk48m`）枚举，倍数/分频与 Linux 从 `clock-mult`/`clock-div` 读的值
等价地来自驱动内的板级表。父时钟边是真实的固件引用（描述符里的 `clocks`），
经 `dev.res().clk("parent")` 解析到真实 provider。

实测启动：`fixed-factor: published clk48m @ 48000000 Hz (parent 24000000 Hz, 2/1)`。

**已知限制**：`kclk` 的 `FixedRate` 不跟踪父时钟变化，派生频率在 probe 时采样
一次；Linux `clk-fixed-factor` 会在父时钟变化时重算。

## Drop / 资源释放

`FixedClockData { _core: Arc<ClkCore> }` —— 唯一字段，持有时钟树存活。

- probe 失败（`ClkBuilder::build` 出错、缺 `phandle`/`clock-frequency`）→
  `Data` 从未构造，局部 `Arc` 就地 drop，时钟树释放。
  **注意顺序**：`Box::leak` + `dev.provide` 在 `Ok(...)` 之前，
  所以一旦注册成功就不会再失败——注册是 probe 的最后一步，这是刻意的，
  避免出现"已注册但 probe 失败"的半状态。
- 设备移除 → `remove()` 空操作 → drop `Data` → `Arc<ClkCore>` 减一。
  **但 lookup 仍然指向它**（`FixedClockLookup.core` 是另一个强引用，且被 leak），
  所以时钟树实际不会释放。这与"boot-static"的选择一致，但意味着
  **移除 fixed-clock 设备后 provider 仍然可用**。已记录在"已知限制"。

## scmi_clock（RK3588 SD 时钟的第二 provider）

`src/scmi_clock.rs`（feature `scmi-clock`）是同一家族里的第二个 provider
驱动，形状与 `fixed_clock` 相同（发布 `ClkProvider`、`Box::leak` lookup、
boot-static），但能力来自固件而不是 DT 常量：

```text
DT 节点                                  probe
  scmi { compatible="arm,scmi-smc";       └─ ScmiClockDriver::probe
         shmem=<&scmi_shmem>;                 ├─ fw().u32("reg") == 0x14？（clock protocol 才认领）
         arm,smc-id=… }                       ├─ iomap_region(SCMI_SHMEM_PADDR, 0x100)
    protocol@14 { reg=<0x14>;                 ├─ CLOCK_ATTRIBUTES(9)：agent 不答 → Unsupported
      #clock-cells = <1>; }                   ├─ ClkBuilder("scmi_cclk_sd"/"scmi_hclk_sd", ScmiClkOps)
  }                                           └─ provide::<ClkProvider>(phandle, lookup)
```

- **枚举侧配合**（drivers/init）：无 compatible 但带 `#clock-cells` 的裸
  provider 节点被合成 `compatible = "clock-provider"` 身份枚举，驱动据此
  认领 `protocol@14`；phandle 即 provider key，与消费者
  `clocks = <&scmi_clk SCMI_CCLK_SD>` 的 phandle 天然对齐，`ClkIndex`
  就是 SCMI 时钟 id（CCLK_SD=9，HCLK_SD=23）。
- **传输**：SMT 共享内存 + SMCCC fast call（`karch::smc_call`）。
  SMC id 与 shmem 基址是板级事实（`SCMI_SMC_ID` / `SCMI_SHMEM_PADDR`
  构建参数，`RTC_PADDR` 同款先例）——protocol 子节点自身无法携带父节点的
  `arm,smc-id`。
- **能力边界**：只做 `rate_get` / `enable` / `disable`。`set_rate`
  明确 `NotSupported`（本 slice 的消费者不改上游频率，DW 控制器内部分频）；
  未知 id 返回 `None`，其消费者维持原状（相当于没有 provider）。
- 执行上下文：probe 可睡眠（SMC fast call 同步返回，无阻塞等待）；
  `recalc_rate`/`enable` 会在 kclk 持锁上下文里发 SMC —— 与 Linux
  `clk-scmi` 的 `.is_atomic = false` 语义一致。
