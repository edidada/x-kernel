# drv-clk — 设计文档

## 定位

driver-core 模型下的**时钟 provider 驱动**聚合 crate。当前只有一个驱动：
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
DT 节点                              driver-compose::build_descriptor
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
   └─ driver_host::register_clk_provider(lookup)          // ← 唤醒钩子在这里触发
        │
        ▼
   driver_host::ClkRegistry::register_provider
        ├─ kclk::register_provider(phandle, lookup)
        └─ wake_hook(DepSource::clk(phandle))
             └─ driver_core::global_domain().wake(dep)
                  └─ 所有 parked 在这个 phandle 上的消费者被精确重探
```

消费者侧（例如 pl011）只写 `dev.clk("baudclk")`：provider 未注册时
`ClkRegistry::get` 把 `kclk::ClkError::NoProvider` 翻译成
`ServiceError::Deferred(DepSource::clk(phandle))`，域 park 设备。**没有轮询、
没有重试表、没有顺序表。**

## 调用约束 / 执行上下文

- `probe` 由域在锁外的任务上下文调用，可睡眠（`ClkBuilder::build` 分配）。
- `driver_host::register_clk_provider` 要求 `GLOBAL_RUNTIME` 已安装，否则
  **panic**（启动顺序断言）。因为 `driver-compose::init()` 第一行就是
  `install_global()`，而 `fixed_clock::register()` 在其后，这个前提成立。
- `register()` 必须在**消费者驱动之前**调用吗？**不必须**。deferred probe 的
  全部意义就是顺序无关：先 probe 的消费者会 defer，provider 发布时被唤醒。
  compose 里 `fixed_clock` 排在第一位只是为了减少一轮 defer。
- 本 crate 无锁、无原子、无中断处理、无 MMIO。

## 设计决策

### `Box::leak` 是刻意的，不是泄漏 bug

```rust
let lookup: &'static FixedClockLookup = Box::leak(Box::new(FixedClockLookup { … }));
driver_host::register_clk_provider(lookup);
```

`register_clk_provider` 的签名要求 `&'static dyn kclk::ClkLookup`，因为
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
`dev.clk("baudclk")` 时走的是 DT `clock-names` → `ClkSpec::by_phandle_index`
的 index 路径，不是 name 路径。

### 名字来自 `clock-output-names`，回退到 `"fixed-clock"`

`firmware.clock_output_name.unwrap_or("fixed-clock")` 是 `&'static str`
（借自 DT blob），因此 `ClkBuilder::new(name, …)` 不需要分配。

### 依赖 `driver_host` 与 `kclk`（登记的例外 E-02c）

provider 驱动的职责**就是**往主机时钟子系统注册。Linux 的
`of_clk_add_hw_provider` 同样是直接调主机 API。为它加一层
`DeviceServices::publish_clk` 能力契约是纯间接——而且 `FixedClockLookup` 要
构造 `kclk::ClkCore`，时钟树的类型本来就属于 `kclk`。
见 `docs/ai/driver-subsystem-boundaries.md` 例外 E-02。

## Drop / 资源释放

`FixedClockData { _core: Arc<ClkCore> }` —— 唯一字段，持有时钟树存活。

- probe 失败（`ClkBuilder::build` 出错、缺 `phandle`/`clock-frequency`）→
  `Data` 从未构造，局部 `Arc` 就地 drop，时钟树释放。
  **注意顺序**：`Box::leak` + `register_clk_provider` 在 `Ok(...)` 之前，
  所以一旦注册成功就不会再失败——注册是 probe 的最后一步，这是刻意的，
  避免出现"已注册但 probe 失败"的半状态。
- 设备移除 → `remove()` 空操作 → drop `Data` → `Arc<ClkCore>` 减一。
  **但 lookup 仍然指向它**（`FixedClockLookup.core` 是另一个强引用，且被 leak），
  所以时钟树实际不会释放。这与"boot-static"的选择一致，但意味着
  **移除 fixed-clock 设备后 provider 仍然可用**。已记录在"已知限制"。
