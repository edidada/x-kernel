# drv-serial — 设计文档

## 定位

driver-base 模型下的串口驱动聚合 crate，含两个 UART 家族、三个注册入口：

| 模块 | feature | id 表 | 说明 |
|---|---|---|---|
| `pl011` | `pl011` | `arm,pl011` | **消费者驱动的 golden 模板**：命名资源、时钟 + 自动 defer、IRQ 的 dev_id 捕获、guards-in-Data、早期 console 收养 |
| `ns16550`（MMIO） | `ns16550-mmio` | `ns16550a` / `ns16550` / `snps,dw-apb-uart` / `mrvl,mmp-uart` | DT UART 节点，寄存器布局从早期 console 的 layout 表解码 |
| `ns16550`（I/O 口） | `ns16550-ioport` | alias `serial-ns16550-ioport` | x86 四个 ISA COM 口，板级静态设备，无固件描述 |

串口是**唯一一个既要当普通字符设备、又要当内核 console** 的类，因此本 crate
承载了框架里最复杂的一条所有权协议（见"stdout 所有权交接协议"）。

## 范围

| 文件 | 内容 |
|---|---|
| `src/lib.rs` | crate 门面 + feature 门控的模块声明 |
| `src/pl011.rs` | `Pl011Driver` / `Pl011Data`（6 字段）/ `SerialCharDev` / `enable_baud_clock` / `register()` |
| `src/ns16550.rs` | `Ns16550MmioDriver` + `Ns16550IoPortDriver`（共享 `SerialData` 与 `publish()`）/ `register_mmio()` / `register_ioport()` |
| `src/probe_error.rs` | `map_driver_error`：`kerrno::KError` → `ProbeError::Fatal(RuntimeError)` |

## 架构

```text
                       Device (driver-base)
                             │
        ┌────────────────────┼────────────────────┐
        │ 命名资源            │ 时钟                │ 中断
        ▼                    ▼                    ▼
   dev.res().iomap(0)         dev.res().clk("baudclk")   dev.res().request_irq(spec,"pl011",closure)
        │                    │                    │  闭包捕获 Arc<SerialPort>
        │              ┌─────┴──────┐             │  （dev_id 模式）
        │              │ provider   │ provider    │
        │              │ 未注册     │ 已注册       │
        │              ▼            ▼             │
        │        Deferred(clk)   ClkHandle        │
        │              │       + prepare_enable    │
        │              │       → EnabledClock      │
        └──────┬───────┴───────────┬──────────────┘
               ▼                   ▼
   take_early_port(&ident) ── 是 stdout? ──► 收养早期 console 实例（Arc 共享）
               │                              │
               │否                            │
               ▼                              ▼
   SerialPort::new_mmio_pl011(...)   register_console_parented(...)
   （冷启动，需要自己的 IoMap）        **不持有 withdrawal guard**
               │                              │
               ▼                              ▼
        publish(&CHAR, ...)            adopt_stdout_guards(irq, regs)
        + char_withdraw guard         （guard 所有权移交给 console 子系统）
               │
               ▼
        Pl011Data { _publication, _irq, _port, _regs, _clk, _enabled_clk }
```

## stdout 所有权交接协议

这是本 crate 存在的理由，也是例外 E-03 的实质内容。**console 的可见性必须
跨越驱动 binding 的生死**——printk 不能在设备移除/rebind 期间断流。因此
stdout 端口走一条与辅助端口**系统性不同**的路径：

| 资源 | 辅助端口（Auxiliary） | stdout 端口（Stdout） |
|---|---|---|
| `SerialPort` 实例 | 冷启动：自己 `dev.res().iomap(0)` + `SerialPort::new_*` | **收养**：`take_early_port(&ident)` 返回与 printk 共享的 `Arc<SerialPort>`，不建新映射（`regs = None`） |
| class 发布 | `publish_char` + `char_withdraw` guard 存进 `Data`，drop 即撤销 | `register_console_parented`，**故意不持有 withdrawal guard** → 发布比 binding 活得久 |
| IRQ guard | 存进 `Data._irq`，drop 即释放 | `adopt_stdout_guards(irq.take(), …)` **移交给 console 子系统** |
| MMIO 映射（仅冷启动端口） | 存进 `Data._regs`，drop 即 unmap | `adopt_stdout_guards(…, regs.take())` **移交** |
| 已使能的时钟 | `EnabledClock` token 存进 `Data._enabled_clk`，drop 时 disable + unprepare | `token.keep_running()` **消费掉 token**，时钟持续运行 |

**为什么必须移交而不是丢弃**：一个活得比它的 IRQ 更久的发布，会读起来像
"一个可用的 console，但永远收不到输入"。所以发布与其依赖的 guard 必须一起
转移所有权。

三个驱动的移交形状不同，且**都是正确的**：

- `pl011`：移交 `irq` + `regs`（它有中断，冷启动时有映射）；
- `ns16550-mmio`：移交 `regs`，`irq` 传 `None`（当前不注册输入中断）；
- `ns16550-ioport`：两个都传 `None`（I/O 口无需映射；ioport stdout 就是被收养
  的早期端口，binding 不拥有任何 guard）。

## 调用约束 / 执行上下文

- `probe` / `remove` 由域在锁外的任务上下文调用，可睡眠。
- **IRQ handler 闭包在硬中断上下文运行**。它只做
  `handler_port.handle_input_irq()`——确认并收集输入、唤醒挂着的 tty waker，
  不做任何设备模型调用、不取域锁、不分配。
- `dev.res().clk()` 可能返回 `Deferred`，因此 probe **必须**能安全地被重跑：所有已
  获取的 guard 都是局部值，`?` 提前返回时按 Rust 逆序 drop。`enable_baud_clock`
  返回的 token 在后续每个可失败步骤里都保持存活，只有 probe 完全成功才决定
  token 的去向（`keep_running` 或存进 `Data`）。
- `take_early_port` 只能成功一次（它把早期端口**取走**）。因此 stdout 节点的
  probe 若失败后重试，第二次会走冷启动路径、建第二份映射指向同一个 UART。
  当前依赖"stdout probe 不会失败"（它只需要 `reg` 与时钟）。
- x86 I/O 口变体不需要映射：端口范围从 `dev.resources()` 的
  `ResourceKind::IoPort` 里读。

## 状态机

本 crate 没有自己的状态机。设备状态由 `driver-base` 的 `DeviceState` 负责，
端口角色（`SerialRole::{Stdout, Auxiliary}`）在 `SerialPort` 构造时确定且不可变。

时钟有一个两段式的所有权状态，值得单独记：

```text
dev.res().clk(name) ─► ClkHandle（获取，未使能）
      │
      ▼
prepare_enable() ─► EnabledClock（token：代表"已 prepare + 已 enable"这笔账）
      │
      ├─ 辅助端口：token 存进 Data._enabled_clk ─► Drop 时 disable + unprepare
      └─ stdout：  token.keep_running() 消费掉   ─► 时钟永久保持使能
```

## 设计决策

### `_publication` 是 `Data` 的**第一个**字段

Rust 按声明顺序 drop 字段。发布必须**先于**它投影的资源的释放被撤销，
否则消费者会在 `/dev/ttyX` 已经指向一个失效端口时还能打开它。
`Pl011Data` 的完整顺序：

```rust
pub struct Pl011Data {
    _publication: Option<Publication>,  // 1. 先撤销可见性
    _irq: Option<DevIrq>,                 // 2. 停中断（它服务下面的映射）
    _port: Arc<SerialPort>,               // 3. 释放端口引用
    _regs: Option<IoMap>,                 // 4. 解映射
    _clk: Option<ClkHandle>,              // 5. 放时钟引用
    _enabled_clk: Option<EnabledClock>,   // 6. 平衡 prepare/enable
}
```

`remove()` 是空的——不是遗漏：串口没有需要显式 quiesce 的 DMA 或队列，
字段顺序已经表达了正确的释放序。这与 `drv/virtio/src/blk.rs` 形成对照，
那里 `remove()` 必须显式 drain I/O。

### handler 恒返回 `Handled`（已知正确性债务）

`pl011` 的 handler 不先查 RIS（raw interrupt status）就 ack 并报 `Handled`。
在所有当前支持的 DT 上 PL011 是**独占 SPI**，因此无害；在**共享中断线**上它
会吞掉别人的中断。修法明确（先查 RIS → 再 ack → 再报 `Handled`），但改 console
中断路径的风险目前大于这个假设场景的收益。

这一项在 `docs/driver-framework-review.md` §8.3.1 里从"C 类：QEMU 掩盖"被
**改判为已知正确性债务**——它不是被测试环境掩盖，而是当前硬件配置下不触发。

### `SerialCharDev::irq()` 返回 `None`

class 层的 `Device::irq()` 是历史遗留的 introspection 字段。串口的 IRQ 由
`Data._irq` 拥有，不通过 class 暴露——因此这里返回 `None` 是准确的，
不是没实现。

### 依赖子系统与 family 库（不再是 E-03 例外）

早期 console 收养与 stdout 身份交接曾被登记为 `console_driver` 的依赖例外。
现在它们都是正常的向下调用：`serial_core::take_early_port`（family 库）与
`subsys-char-console`（char 类旁的子系统服务）。本 crate 的 host 依赖只剩
serial-core 测试用的 `khal`（E-03），生产代码不碰 host 运行时。
`ns16550` 的寄存器布局不再跨 crate 重扫 FDT，而是读本设备自己的
`dev.fw()` 快照。

stdout 所有权交接协议完整写在本文档里（这才是 review 真正需要的东西：
`df5f15ead`、`6f7d44aea`、`0cfb1ad88` 三次修复都发生在这条协议上）。

## Drop / 资源释放

见"stdout 所有权交接协议"与"`_publication` 是第一个字段"。

补充两点：

- `Option<…>::take()` 是移交的实现手段：`adopt_stdout_guards(irq.take(),
  regs.take())` 之后 `Data` 里对应字段是 `None`，drop 时不会二次释放。
- stdout 端口的 `_publication` 永远是 `None`（`register_console_parented`
  不返回 guard，ns16550 的 `publish()` 显式 `.map(|_| None)`）。因此 stdout
  binding 被移除时**发布仍然存在**——这是 console 连续性的定义，不是泄漏。
