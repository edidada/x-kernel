# drv-serial — 安全与可靠性分析

## Scope

`drivers/drv/serial/src/{lib,pl011,ns16550,probe_error}.rs`（552 行）。

串口驱动坐在两条真实的外部边界上：**固件描述的 UART 窗口**（地址、大小、
中断、时钟）与 **UART 收到的字节**（串口 console 输入最终进入内核的命令行
解析与 tty 层）。它同时也是唯一承载"发布比 binding 活得久"这条所有权协议的
驱动，因此移除路径的错误会直接表现为 console 断流或悬垂发布。

## 信任模型

| 边界 | 谁在外面 | 假设 |
|---|---|---|
| 设备树 → 本 crate | `reg`（base/size）、`interrupts`、`clocks`/`clock-names` | **半可信**。地址合法性由 `memspace` 判断；本 crate 额外依赖一个**编译期表**把 base 映射到寄存器布局（见 T-02） |
| 板级静态表 → ioport 变体 | `COM_PORTS: [u16; 4]`（在 `driver-init`）经 `ResourceKind::IoPort` | 可信（编译期常量） |
| **UART 硬件 → 本 crate** | RX FIFO 里的字节、LSR/RIS 状态位 | **不可信**。对端（宿主机串口、物理串口上的设备）可以发任意字节，也可以让状态位呈现任意组合 |
| 设备固件快照 / `serial_core` → 本 crate | `take_early_port` 返回的 `Arc<SerialPort>`、`SerialMmioLayout::from_props`（`reg-shift`/`reg-io-width`） | 可信（同一内核的启动集成层与 family 库） |
| 本 crate → tty / class 消费者 | `SerialCharDev::{read, write, flush}` | 消费者可信；但 `read` 返回的字节来自不可信的 UART |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| **设备树** | `dev.res().mmio(0)` / `dev.res().irq(0)` / `dev.res().clk(name)` / `dev.descriptor().location` | `MmioRegion{base,size}` 直接决定 `SerialPort` 的窗口；`interrupts` 决定 IRQ 路由；`clocks` 决定波特率基准 |
| **UART 寄存器（MMIO / PIO）** | `SerialPort::{handle_input_irq, read_data, write_data}`（在 `serial-core`） | 本 crate 只在 probe 里构造 `SerialPort` 并注册 handler，寄存器访问本身在 `serial-core`。但**中断 handler 的行为由本 crate 决定**（返回 `Handled`/`NotHandled`） |
| **UART RX 数据** | `SerialCharDev::read` → tty → 用户态 / 内核命令行 | 不可信字节流。本 crate 只做透传（`self.0.read_data(buf)`），不解析 |
| 用户内存 | `read(&self, buf: &mut [u8])` 的 `buf` | 是内核缓冲（tty 层提供），不是裸用户指针；用户拷贝在 tty/POSIX 层 |
| DMA | 无 | 串口是 PIO |

## unsafe 代码清单

生产代码 **3 处** `unsafe`，全部是"把 `IoMap`/端口号交给 `serial-core` 的
`SerialPort` 构造器"：

| 位置 | 调用 | 依赖的不变量 | 由谁保证 |
|---|---|---|---|
| `pl011.rs:115` | `SerialPort::new_mmio_pl011(VirtAddr::from_usize(regs.as_ptr()), paddr, mmio.size, Auxiliary)` | `regs` 恰好覆盖固件声明的 `mmio` 窗口；PL011 寄存器按 spec 是 32 位对齐；`regs` 在端口整个生命周期内存活 | 紧邻上一行的 `dev.res().iomap(0)?` 建立了映射；`regs` 被 move 进 `Pl011Data._regs`，字段顺序保证它在 `_port` **之后**释放 |
| `ns16550.rs:141` | `SerialPort::new_mmio_ns16550(vaddr, paddr, mmio.size, Auxiliary, layout.reg_shift, layout.reg_width)` | 同上，外加 `reg_shift`/`reg_width` 与真实硬件一致 | 同上；布局来自本设备自己的固件快照（`dev.fw()` 的 `reg-shift`/`reg-io-width`，经 `SerialMmioLayout::from_props` 解码，缺失时按 Linux 8250 默认 stride=1/width=8）；非法宽度就 `Fatal(Invalid)`（见 T-02） |
| `ns16550.rs:204` | `SerialPort::new_ioport_ns16550(port_base, Auxiliary)` | 端口范围来自板级静态描述符，且被本设备独占 | `driver-init::enumerate_static_devices` 硬编码 `COM_PORTS` 与 `size: 8`；无固件参与 |

**收养路径没有 unsafe**：`take_early_port(&ident)` 返回已经构造好的
`Arc<SerialPort>`，映射由启动期的 console 初始化建立。

`IoMap::as_ptr()` 本身是 safe 的（`driver-base` 把 soundness 责任收在
`IoMap::new` 那个 `unsafe fn` 里）；这里的 `unsafe` 是 `SerialPort` 构造器
要求的，因为它接受一个裸虚拟地址而类型上无法证明该地址的有效范围。

## 内存安全不变量

1. **`IoMap` 的生命周期覆盖 `SerialPort` 的使用期**。`Pl011Data` 的字段顺序
   是 `_port` 在 `_regs` 之前 → drop 时 `_port` 先释放，映射后释放。
   `SerialData`（ns16550）同理：`regs` 是最后一个字段。
2. **stdout 端口的 guard 要么在 `Data` 里、要么已移交给 console 子系统，
   绝不两边都在，也绝不两边都不在。** 移交用 `Option::take()` 实现，
   因此 `Data` 里对应字段变 `None`，drop 不会二次释放。
3. **stdout 的 class 发布没有 withdrawal guard**（不变量的另一面）：
   发布比 binding 活得久，因此它依赖的 IRQ 与映射**必须**一起移交（不变量 2）。
   一个活得比它的 IRQ 更久的发布 = 一个"看起来可用但永远收不到输入"的 console。
4. **`EnabledClock` token 恰好被消费一次**：stdout 走 `keep_running()`
   （token 被消费，时钟保持使能），辅助端口走 `Data._enabled_clk`
   （drop 时 disable + unprepare）。两条路径互斥，由 `match port.role()` 决定。
5. **`SerialPort` 的 `Arc` 引用计数是 printk 与驱动之间的唯一协调机制**。
   收养路径下 printk 持有一份强引用，因此即使 `Data._port` 释放，早期 console
   仍能继续输出。

## 线程安全

- `Pl011Driver` / `Ns16550*Driver` 是零大小类型，`Send + Sync`。
- `Pl011Data` / `SerialData` 的每个字段都是 `Arc` 或 guard 类型，`Send + Sync`
  由字段继承（`DriverOps::Data: Send + Sync` 是契约要求）。
- **IRQ handler 闭包捕获 `Arc<SerialPort>`**（不是 `Weak`）——与 virtio 驱动的
  `Weak` 模式不同。理由：串口的 handler 与端口同生命周期，`Data._irq` 在
  `Data._port` 之前释放，所以 handler 不可能在端口失效后被调用。
  **代价**：handler 持有强引用期间，`Arc<SerialPort>` 不会归零。stdout 路径
  下这是必要的（printk 也持有）；辅助端口下这意味着"IRQ 释放之前端口不会销毁"，
  正是想要的顺序。
- handler 在**硬中断上下文**运行，只调 `handle_input_irq()`；不取域锁、
  不分配、不调设备模型。`SerialPort` 内部用 `SpinNoIrq` 保护（在
  `serial-core`）。
- `take_early_port` 是"取走"语义（`Option::take` 风格），因此并发 probe 两个
  声称是同一 stdout 的节点时只有一个能收养成功。**当前不构成风险**：DT 里
  stdout-path 只指向一个节点。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 共享中断线上，pl011 handler 恒返回 `Handled` 并 ack，吞掉同线其他设备的中断 | 中 | 平台把 PL011 与别的设备共享一条 SPI | **未修复（已知正确性债务）**。当前所有支持的 DT 上 PL011 是独占 SPI，因此不触发。修法明确：先查 RIS → 只有本设备的位被置位才 ack → 才返回 `Handled`。**残余风险**：移植到共享线平台时会出现"另一个设备的中断丢失"，表现为该设备卡死而非串口异常，诊断方向容易错。见 `docs/driver-framework-review.md` §8.3.1 |
| T-02 | 固件给出一个 `reg.base`，它在 `ns16550_mmio_layout_for_paddr` 的表里对应**错误的** `reg_shift`/`reg_width`，驱动按错误步长访问寄存器 | 高 | DT base 与真实硬件不符，或表里的平台条目写错 | **部分实现**：表查不到时 probe 返回 `Fatal(Invalid)`，**不会**用默认布局硬上（这是关键的失败方向）。**残余风险**：表里有错误条目时无法检测——按错误 `reg_shift` 访问会读到相邻寄存器，可能改写 UART 的 FCR/MCR 而不是 LCR。加固建议：把布局从"按物理地址查表"改成从 DT 的 `reg-shift`/`reg-io-width` 属性读取（Linux 的 `ns16550a` 绑定就是这么做的），让固件描述成为唯一来源 |
| T-03 | 固件给出的 `mmio.size` 小于真实寄存器窗口，`SerialPort` 访问超出映射 | 高 | 畸形 DT | **已实现**：`dev.res().iomap(0)` 映射的正是 `mmio.size`，`SerialPort` 收到的 size 与映射大小一致；`driver-base::IoMap` 的每次寄存器访问都做 `offset + width <= region.size` 的**硬检查**（越界 panic，不是 UB）。因此后果是响亮 panic 而不是越界访问 |
| T-04 | UART 对端发送海量字节，RX 处理压垮中断或 tty 缓冲 | 中 | 物理串口上的恶意设备、或宿主机串口灌数据 | **不在本 crate 防御**。handler 只确认并收集，唤醒 tty waker；缓冲与背压在 `serial-core` 的 ring buffer 与 tty 层。**残余风险**：本 crate 无速率限制 |
| T-05 | UART RX 字节被当作可信输入解析 | 高 | 串口 console 登录 / 内核命令行 | **不在本 crate**：`SerialCharDev::read` 只做 `self.0.read_data(buf)` 透传，不解析。解析责任在 tty 行规程与命令行解析器，它们必须把串口输入当不可信 |
| T-06 | stdout binding 被移除，但发布与 guard 的移交不完整 → console 发布指向已释放的 IRQ/映射 | 高 | 移除 stdout 设备；或新增一个串口驱动时漏了移交 | **已实现**：不变量 2 + 3。`adopt_stdout_guards` 显式接收 `irq` 与 `regs`，用 `take()` 移交。**残余风险**：这是**手工协议**，编译器不检查完整性。`df5f15ead`/`6f7d44aea`/`0cfb1ad88` 三次修复都在这条协议上，说明它容易做错。加固建议：让 `register_console_parented` 直接接收 guards 作为参数（把"发布"与"移交"合成一个不可拆分的调用），而不是分两步 |
| T-07 | stdout probe 失败后重试，`take_early_port` 已被取走 → 第二次走冷启动路径，对同一 UART 建第二份映射与第二个 `SerialPort` | 中 | stdout 节点的 probe 失败（缺时钟 provider、映射失败） | **未防御**。后果是两个 `SerialPort` 实例操作同一个 UART，printk 与驱动各持一份状态。**残余风险**：当前 stdout probe 只需要 `reg` 与时钟，失败概率低；但 `dev.res().clk()` 的 `Deferred` 路径**会**导致 probe 提前返回，此时 `take_early_port` 已经执行过了。加固建议：把 `take_early_port` 移到所有可失败步骤**之后**，或在失败路径上把端口还回去 |
| T-08 | 时钟频率错误导致波特率偏移 | 低 | DT `clock-frequency` 错误（`drv/clk` 的 T-01） | **不在本 crate 防御**。`enable_baud_clock` 只负责 prepare/enable，分频计算在 `serial-core`/`SerialPort` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `dev.res().clk()` 返回 `Deferred` | baud clock 的 provider 尚未注册 | probe 提前返回，已获取的局部 guard 逆序 drop | 设备 parked；provider 发布时被精确唤醒。**注意 T-07：若 `take_early_port` 已执行，重试会走冷启动路径** | 3 | 这是设计行为（`-EPROBE_DEFER` 等价物）。token 在每个可失败步骤里保持存活，只有全部成功才决定去向 |
| F-02 | `dev.res().iomap(0)` 失败 | 地址范围非法 / 无内存 | probe `Fatal` | 设备 `Unclaimed`；若它是 stdout，printk 仍走早期 console（收养路径不依赖这次映射） | 3 | `?` 传播，`map_driver_error` 分类 |
| F-03 | `ns16550_mmio_layout_for_paddr` 返回 `None` | 该 base 不在编译期表里 | probe `Fatal(Invalid)` | 该 UART 不被认领 | 3 | **刻意失败而不是用默认布局**（T-02） |
| F-04 | `dev.res().irq(0)` 失败 | 描述符没有 `interrupts` | pl011：`irq = None`，**继续 probe**；端口只能轮询/被 printk 使用 | 该端口收不到输入中断 → `/dev/ttyX` 的 read 不会因输入而唤醒 | 3 | pl011 把 `Err(_)` 映射成 `None` 而不是失败——这是刻意的：无中断的 UART 仍然是有用的输出设备。**残余风险**：这个降级是静默的，只有 `log::info` 里的 `irq=usize::MAX` 是线索 |
| F-05 | `request_irq` 失败 | 线已被独占 / virq 耗尽 | probe `Fatal`（`?` 传播） | 设备 `Unclaimed` | 3 | 与 F-04 不同：有 `interrupts` 属性但注册失败被视为致命，因为那意味着路由冲突 |
| F-06 | `publish_char` / `register_console_parented` 失败 | class 注册表查重、devnode index 耗尽 | probe `Fatal` | 辅助端口：设备 `Unclaimed`。stdout：**console 不切换到运行时实例**，printk 继续用早期 console（`ktty::tty::try_handoff_console` 会记录降级） | 2 | `map_driver_error` 分类；stdout 的降级路径在 `ktty` 里可见（实测日志 `tty: runtime stdout read failed (BadState), using boot port`） |
| F-07 | `prepare_enable()` 失败 | 时钟树状态错误 | probe `Fatal` | 设备 `Unclaimed` | 3 | token 语义保证：失败时 `ClkHandle` 仍持有 prepare 前的状态，drop 不会错误地 disable |
| F-08 | stdout binding 被移除 | 域 remove（热插拔 / 测试） | `remove()` 空操作；`Data` drop 时 `_publication` 是 `None`（stdout 不持有 guard）→ **发布保留** | console 继续工作。这是设计目标 | 4 | 不变量 3。若将来要求 stdout 可真正移除，需要同时撤销发布并让 printk 回退到早期 console |

## 故障管理

- probe 的所有失败经 `crate::probe_error::map_driver_error` 变成
  `ProbeError::Fatal(RuntimeError)`；**唯一的 `Deferred` 来源是 `dev.res().clk()`**
  （由 `Device` 自动折叠 `ServiceError::Deferred`），驱动代码里看不到
  `Deferred` 的构造。
- `remove()` 是空的且不返回 `Result`（`DriverOps::remove` 签名如此）。
  串口的全部释放语义在字段顺序里。
- 本 crate 不 panic。`IoMap` 的越界/未对齐 panic 在 `driver-base`，
  是 T-03 的应对手段而不是本 crate 的故障。
- 日志：成功时一条 `log::info`（含 paddr、时钟频率、**硬件线号**而不是
  `DevIrq` 的号——因为路由式 spec 在 domain 映射解析前 `DevIrq` 号是 0）。

## 隐私分析

**串口是双向的用户数据通道**，这一点与框架里其他驱动不同：

- **输入**：UART RX 字节 → tty → 用户态。本 crate 只透传，不落盘、不日志。
  但要注意：如果串口是 console，**内核日志会从同一个端口输出**，因此日志里
  不能出现敏感信息——这是全局日志纪律，不是本 crate 的属性。
- **命令行**：通过串口 console 输入的内核命令行参数会被解析，属于不可信输入
  （T-05）。
- 本 crate 自身不处理、不存储任何用户数据。

## 已知限制

1. **pl011 handler 恒返回 `Handled`**（T-01），共享中断线上会吞中断。
   已登记为正确性债务。
2. **NS16550 寄存器布局按物理地址查编译期表**（T-02），不读 DT 的
   `reg-shift`/`reg-io-width`。新平台必须改表。
3. **stdout 所有权移交是手工协议**（T-06），编译器不检查完整性。
4. **`take_early_port` 在可失败步骤之前执行**（T-07），stdout probe 失败重试
   会产生第二个 `SerialPort`。
5. **无中断的 UART 静默降级**（F-04），只有日志可查。
6. **stdout 发布不可撤销**（F-08），这是 console 连续性的代价。
7. **无 DMA、无 flow control、无 termios 完整支持**（`SerialCharDev` 只实现
   `read`/`write`/`flush`）。
8. **依赖 `subsys-char-console` 与 `serial-core`**：前者是 char 类旁的
   子系统服务，后者是 family 库；`drv/**` 的 host 依赖只剩 serial-core
   测试用的 `khal`（例外 E-03），生产代码不碰 host 运行时。

## 审计清单

- [ ] `Data` 的字段顺序是否仍是"发布 → IRQ → 端口 → 映射 → 时钟 → 使能 token"？
      新增字段时要放进正确的相对位置（不变量 1）。
- [ ] 新增的 stdout 路径是否把**每一个** binding 拥有的 guard 都移交了？
      （不变量 2/3，T-06）漏一个就是一个悬垂发布。
- [ ] 新增的可失败步骤是否放在了 `take_early_port` **之后**？如果是，
      T-07 的双实例风险要重新评估。
- [ ] IRQ handler 闭包是否仍然只做 ack + 收集 + 唤醒？有没有引入分配、
      取锁、或设备模型调用？（硬中断上下文）
- [ ] handler 捕获的是 `Arc` 还是 `Weak`？本 crate 用 `Arc` 是有理由的
      （见"线程安全"）；改成 `Weak` 需要重新论证释放顺序。
- [ ] 新增的 `unsafe` 是否有紧邻的 `SAFETY:` 说明，指名哪个 guard 保证
      生命周期？
- [ ] `EnabledClock` token 的去向是否在**所有可失败步骤成功之后**才决定？
      提前 `keep_running()` 会让失败路径泄漏一个使能着的时钟。
- [ ] 新的 UART 家族是否需要新的 layout 条目？表查不到必须返回 `Fatal`，
      **不得**回退到默认布局（T-02 的失败方向）。
