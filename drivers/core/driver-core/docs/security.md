# driver-core — 安全与可靠性分析

## Scope

`drivers/core/driver-core/src/`：`bus.rs`、`driver.rs`、`device.rs`、`error.rs`、
`resource.rs`、`provider.rs`、`domain/{mod,registry,probe,dependency,lifecycle}.rs`。
测试 `domain/tests.rs` 不在分析范围内，但其中的 `MockServices` 是信任模型的
一个具体实例。

## 信任模型

| 边界 | 谁在外面 | 本 crate 假设什么 |
|---|---|---|
| 固件 → 描述符 | DT / ACPI / PCI 配置空间（`driver-compose`、`bus/pci`、`firmware/of` 转换后交给 `add_device`） | **半可信**：地址、大小、中断号、compatible 字符串都可能与真实硬件不符或被篡改。本 crate 只做结构校验，不做"这段物理地址真的是 MMIO"的判断——那是 `driver-host` + `memspace` 的职责 |
| `DriverOps` 实现 → 域 | `drv/**` 里的驱动 | **可信内核代码**。域不防御驱动的错误，但把驱动能造成的破坏限制在"这一个设备"内（generation 守卫、独占移除声明） |
| `DeviceServices` 实现 → 域 | `driver-host::XKernelRuntime` 或测试 mock | **可信**。契约要求返回的 guard 携带正确的 `ReleaseHook` |
| 域 → 观察者 | `DeviceLifecycleObserver` 实现 | 回调在锁外运行，**不得重入域**。这是文档约束，不是类型约束 |

关键的分层判断：本 crate 是**模型**，它不做硬件访问，因此它的 soundness 责任
集中在两处——`IoMap` 的 volatile 访问器，以及"guard 的释放钩子恰好被调用一次"。

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| 设备树 / ACPI | `DeviceDomain::add_device(DeviceDescriptor)` | `MmioRegion{base,size}`、`IoPortRange{base,size}`、`IrqSpec::FirmwareRoute{hwirq,controller,domain,trigger}`、`ClkSpec{phandle,index}`、`compatibles: Vec<String>` 全部来自固件 |
| PCI 配置空间 | 同上（`bus/pci::discover` 产出） | `PciIdentity{vendor_id,device_id,class,subclass}`、BAR 派生的 `MmioRegion`、MSI-X 派生的 irq |
| 板级静态表 | 同上（`driver-compose::enumerate_static_devices`） | x86 COM 口 `IoPortRange{base, size: 8}`、ramdisk alias |
| MMIO 寄存器 | `IoMap::read*/write*` | 本 crate 唯一的硬件触点，且是**写出去的**——读回来的值不进入模型状态 |
| 用户内存 / 用户指针 | 无 | 本 crate 不接受任何用户态指针 |
| DMA 缓冲 | 无（`CoherentMem` 只是所有者句柄） | 分配与映射在 `driver-host`/`kdma` |
| 中断上下文 | **不接受** | 见"线程安全" |

固件输入的校验分工：

- **本 crate 校验**：`add_device` 的父设备存在性与可用性
  （`AddDeviceError::ParentMissing` / `ParentUnavailable`）、bus instance 存在性
  与类型一致性（`BusInstanceMissing` / `BusInstanceTypeMismatch`）、
  `DeviceLocation::FirmwareNode{id}` 的唯一性（由 compose 侧的单调分配器 +
  耗尽拒绝保证）。
- **本 crate 不校验、由下游校验**：`MmioRegion` 是否与真实硬件相符、是否与其他
  设备重叠（`driver-host` → `memspace`）；`hwirq` 是否在该 controller 的合法区间
  （`kirq` 的 domain 映射，失败返回 `ResError`）；`ClkSpec.phandle` 是否存在
  （`ClkRegistry`，不存在则 `ServiceError::Deferred`，不是错误）。

## unsafe 代码清单

生产代码里 **11 处** `unsafe`，全部集中在 `device.rs` 的 MMIO 访问路径：

| 位置 | 依赖的不变量 | 为什么安全 | 由谁保证 |
|---|---|---|---|
| `device.rs:205` `IoMap::new`（**唯一的 `pub unsafe fn`**） | `vaddr` 指向一个至少 `region.size` 字节、在本句柄生命周期内有效的映射，且调用方拥有该映射（配对最终的 release） | 这是把裸地址变成受检句柄的唯一入口；标记 `unsafe` 正是为了让"任意 `usize` 到达 volatile 访问器"在类型上不可能 | 全树两个调用点：`driver-host/src/lib.rs:182`（provider 刚 `map_mmio` 成功）、`domain/tests.rs:101`（mock，`release = None`） |
| `device.rs:252` `access_ptr` 末尾的 `as_ptr().add(offset)` | `offset + size <= region.size` 且 `vaddr + offset` 按 `size` 对齐 | 两个条件在同一函数内**先检查后计算**：对齐不满足 panic，越界 `assert!` | `access_ptr` 是私有函数，8 个访问器全部经过它 |
| `device.rs:259/268/277/286` `read_volatile` | 指针有效且对齐 | `access_ptr` 已保证；`read8` 无对齐要求 | 同上 |
| `device.rs:295/302/309/316` `write_volatile` | 同上 | 同上 | 同上 |

**对齐检查是硬 panic，不是 `debug_assert`。** 旧模型的 `device_res::Io` 用
`debug_assert!`，release 构建下未对齐的 `read_volatile::<u32>` 直接是 UB；
本 crate 改成总是检查（`access_ptr` 里带 `CG-021` 例外说明的 `panic!`）。

`domain/tests.rs` 另有 2 处 `unsafe`（mock 的 `IoMap::new`、把
`Arc<MockServices>` 泄漏成 `&'static`），仅 `cfg(unittest)`。

## 内存安全不变量

1. **`IoMap` 的 `region` 不可变且与 `vaddr` 同时确立**。构造后没有任何 API 能改
   `region.size`，因此 `access_ptr` 的边界检查不可能被后续操作绕过。
2. **每个 guard 的 `ReleaseHook` 至多跑一次**。`IoMap::drop` 用
   `self.release.take()`；`DevIrq`/`CoherentMem`/`ClkHandle`/`EnabledClock` 同构。
3. **`Data` 的装箱与拆箱由同一个驱动类型完成**。`AnyDriver::remove` 里
   `data.downcast_mut::<P::Data>()` 失败会 panic（带 `CG-021` 例外说明），
   而它不可能失败：域只把 binding 的 `Data` 交回**创建它的那个** `RegisteredDriver`
   （`entry.binding.driver` 与 `entry.binding.data` 在同一把锁下同时写入）。
4. **陈旧 probe 结果不会落进 entry**。提交前比对 `lifecycle.generation`；
   不等就 drop 掉那个 `Data`。没有这条，"拆掉再重加"的设备会被上一个 probe 的
   guards 覆盖，两个 `Data` 同时对同一份硬件负责。
5. **`DeviceState` 的迁移只在锁内发生**，且 `Removing` 只能由持有独占声明的
   调用者往前推。因此不存在"两个线程同时跑 teardown"。

## 线程安全

- `DeviceDomain` 是 `Sync`：内部一把 `SpinNoPreempt` + 一个 `SpinNoPreempt<bool>`
  泵标志 + 一个 crate 级 `AtomicU64` id 分配器。
- **禁抢占、不关中断**。因此所有域 API 只能在任务上下文调用。硬中断里调用会
  在同一把锁上自死锁。这条约束靠文档 + review 保证，类型上没有 barrier
  （与 `kdevice` 旧模型的限制相同）。
- `DriverOps: Send + Sync`，`DriverOps::Data: Send + Sync`——因为中断 handler 与
  work item 通过 `Weak` 共享 `Data`。
- `ReleaseHook = Arc<dyn Fn() + Send + Sync>`、`IrqHandlerFn = Arc<dyn Fn() ->
  IrqAction + Send + Sync>`：都要能跨 CPU 执行。
- probe 泵是串行的（`probing` 标志），因此同一时刻只有一个 probe 在锁外运行。
  这不只是性能考虑：`driver-host` 的 MMIO lease collector 是单槽的，依赖这个
  串行化。
- 移除可以与 probe 并发，但结局是确定的：`Probing` 拒绝移除（`Busy`），
  probe 提交时 generation 已变则丢弃结果。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 固件给出一个 `MmioRegion`，其 `base+size` 覆盖内核自身映射；驱动 `iomap` 后写寄存器 → 破坏内核内存 | 高 | 被篡改的 DT / ACPI，或固件描述与真实硬件不符 | **已实现**：本 crate 只按 `region.size` 做访问边界检查，不判断物理地址合法性；映射由 `driver-host` 经 `memspace` 建立，`memspace` 拥有物理地址空间的分配与重叠检测。**残余风险**：若 `memspace` 允许映射一段与内核镜像重叠的物理地址，本层无法发现。加固建议在 `driver-host`/`memspace` 侧拒绝已知保留区 |
| T-02 | 驱动用超出实际寄存器窗口的 `offset` 访问 `IoMap` | 高 | 驱动 bug 或转写错误 | **已实现**：`access_ptr` 用 `checked_add` 防溢出 + `assert!(end <= region.size)`，越界立即 panic 而不是静默访问相邻寄存器 |
| T-03 | 驱动以未对齐 offset 做 `read32`/`write64` | 高 | 转写 Linux 驱动时把寄存器偏移算错 | **已实现**：`access_ptr` 总是检查 `is_multiple_of(size)`，不满足即 panic。旧模型此处是 `debug_assert`（release = UB），本 crate 已修正 |
| T-04 | 设备被移除后，上一轮 probe 的结果被提交，导致两份 `Data` 同时对同一硬件负责（双重释放 IRQ / 双重 unmap） | 高 | 移除与 probe 并发 | **已实现**：`lifecycle.generation` 在每次 teardown 自增；提交时不匹配就 drop 陈旧 `Data` 并 `log::warn`。回归测试 `removing_probing_device_rejects_with_busy`、`wake_during_probe_window_does_not_lose_the_device` |
| T-05 | 同一设备被两条路径同时移除，teardown 跑两遍 | 中 | 热插拔事件与手动移除竞争 | **已实现**：`Removing` + `remove_retryable` 的独占声明——恰好一个调用者能在锁内翻回 `false`，其余看到 `Busy`；`Removed` 上再移除返回 `BadState`。测试 `removing_twice_rejects_with_bad_state` |
| T-06 | 父设备先于子设备被拆，子设备的驱动仍引用父设备的硬件（PCI bridge eject 后 endpoint 还在跑 DMA） | 高 | 桥设备移除 | **已实现**：`remove_device` 第 1 相先翻 `Removing` 关闭拓扑入口，第 2 相 children-first 级联（device children ∪ 桥接的 bus instance 成员）；任一子设备失败则**中止**，父设备停在 `Removing` 且 binding 完整。测试 `remove_cascades_children_first`、`parent_teardown_waits_for_probing_child` |
| T-07 | provider 在"驱动检查依赖"与"域记录 Deferred"之间注册，唤醒丢失，设备永久 parked | 中 | 启动期时钟 provider 与消费者并发探测 | **已实现**：`dep_generations` 对每个 `DepSource` 计数，**即使无设备 parked 也自增**；提交 Deferred 时发现 generation 前进就重新入队。测试注入 `mid_check_hook` 精确复现该窗口 |
| T-08 | 依赖永不发布，设备静默停在 Deferred，故障不可见 | 中 | 固件引用了一个不存在的 clock phandle | **已实现**：`finish_probe_phase(false)` + `deferred_report()` 在探测阶段结束时把卡住的设备 `log::warn` 出来并降级为 `Unclaimed`。实测启动日志可见 `device 70 gave up waiting on dependency clk:66 (provider never registered)` |
| T-09 | 后注册的驱动抢走一个已被别的驱动认领的设备 | 中 | 驱动注册顺序变化 | **已实现**：`select_batch` 跳过 `Active`/`Removing`/`Removed`；`attempted` 集合防止同一个失败驱动被反复选中。测试 `late_driver_registration_claims_unclaimed_device`、`late_driver_registration_skips_unmatched_unmatched` |
| T-10 | 两个 `DeviceDomain` 同时存活时设备 id 撞号，以 owner id 为键的外部注册表（class 发布、devnode）被跨域误撤 | 中 | 单元测试并行、或未来多域 | **已实现**：id 从 crate 级 `DEVICE_ID_ALLOCATOR` 分配，跨域唯一。测试 `device_ids_are_unique_across_domains` |
| T-11 | 观察者回调在锁外重入域（在 `device_removed` 里调 `remove_device`） | 中 | 观察者实现错误 | **部分实现**：契约在 `DeviceLifecycleObserver` 的文档里写明"不得重入"，但类型上没有阻止。**残余风险**：重入会在 `SpinNoPreempt` 上自死锁（禁抢占锁不可重入），表现为 softlockup 而非内存破坏。加固建议：加一个 `in_observer` 标志并在重入时 panic，把死锁变成可诊断的失败 |
| T-12 | `DeviceDescriptor` 的 `compatibles: Vec<String>` 来自固件，超长或超多导致枚举期分配放大 | 低 | 恶意/畸形 DT | **部分实现**：`driver-compose` 侧的 `FirmwareNode{id}` 分配器有 u32 耗尽拒绝；本 crate 不限制字符串长度或 compatible 条数。**残余风险**：启动期内存放大，不影响运行期。加固建议：在 `firmware/of` 解析处限长 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `add_device` 被拒 | 父设备不存在 / 正在拆 / bus instance 缺失或类型不符 | 该设备不进入模型 | 该设备无驱动；`driver-compose` `log::warn` 后继续枚举其余节点 | 3 | 返回结构化 `AddDeviceError`，四种原因可区分；compose 侧不 panic |
| F-02 | 探测返回 `Fatal` | 驱动判定自己不该认领（如 virtio-blk 发现 `DeviceKind != Block`） | 驱动进 `attempted` | 同一轮换下一个候选；穷尽则 `Unclaimed` | 4 | 正常控制流，`log::debug` |
| F-03 | 探测返回 `Deferred` | clock/irq-domain/gpio/regulator provider 未就绪 | 设备 parked | provider 发布时被精确唤醒；阶段结束仍未唤醒则报告 + 降级 `Unclaimed` | 3 | 见 T-07 / T-08 |
| F-04 | 服务未安装就取资源 | `set_services` 之前调用，或某个域没有自己的 runtime | 所有资源获取返回 `RuntimeError::Unsupported` | 该域上所有驱动 probe 失败 → `Unclaimed` | 2 | 刻意的"响亮失败"：`NoServices` 是一个真实类型而不是 `Option::None` 静默路径；启动日志会显示 0 active |
| F-05 | MMIO 访问越界 / 未对齐 | 驱动偏移算错 | **panic** | 内核 panic（`panic = "abort"` 配置下即停机） | 2 | 刻意选择 panic 而不是 UB 或静默截断；panic 消息带 vaddr、offset、要求对齐 |
| F-06 | 级联移除中途失败 | 子设备正在 probe（`Busy`）或已在拆（`BadState`） | 父设备停在 `Removing`，binding 完整，拓扑入口已关 | 父设备不可再挂子设备，但硬件仍被正确持有；下一次 `remove_device` 可独占续做 | 2 | `remove_retryable` + `log::warn`（实测可见 `cascade remove of 77 failed (Busy); teardown ABORTED — retry to continue`） |
| F-07 | `Data` downcast 失败 | 域把 binding 交给了错误的驱动（只能是本 crate 的逻辑 bug） | **panic** | 停机 | 1 | 不变量 3 保证不可达；panic 优于把 `Data` 当错误类型解释（那会是类型混淆） |
| F-08 | 陈旧 probe 结果 | 移除与 probe 并发 | 结果被 drop，guards 就地释放 | `log::warn`，设备保持移除后的状态 | 3 | generation 守卫，见 T-04 |
| F-09 | 固件节点 id 空间耗尽 | 畸形 DT（>4G 节点，实际不可能） | 后续 DT 设备不再枚举 | 已枚举的设备正常工作 | 4 | compose 侧 `next_firmware_node_id()` 拒绝并 `log::error` 一次，不回绕 |

## 故障管理

- **错误词汇刻意分成两套**，防止控制流被混淆：
  - `ProbeError`（`Deferred(DepSource)` / `Fatal(RuntimeError)`）——probe 的
    **控制流**，域据此决定 park 还是换候选；
  - `RuntimeError`（errno 风格，9 个变体）——**服务调用**的失败。
    `ServiceError` 在两者之间做转换，让"某一次获取被 defer"能折叠成
    "这次 probe 被 defer"，而不需要驱动自己写 `should_retry()` 式判断。
- 域 API 一律返回 `Result<_, RuntimeError>`，不 panic——除了三处刻意选择：
  MMIO 越界/未对齐（F-05）、`Data` downcast（F-07）、以及 id 分配器溢出
  （u64，实际不可达）。三处都带 `CG-021-no-panic-on-reachable-failure`
  例外说明。
- 没有锁中毒概念：`SpinNoPreempt` 不可重入，重入即死锁（见 T-11）。
- 移除路径**不可回滚**：一旦翻到 `Removing` 且级联成功，即使 `DriverOps::remove`
  内部出错也继续走到 `Removed`（Linux 的 "remove never fails" 约定）。
  驱动的错误只记日志。

## 隐私分析

无。本 crate 不处理任何用户数据、不接触用户内存、不产生日志之外的可观测输出。
`DeviceSnapshot::identity` 会包含固件 compatible 字符串与 PCI vendor/device id，
这些是硬件描述，经 `/proc/devices`、`/proc/bus/devices` 暴露给本机 root——
与 Linux `/proc/bus/pci/devices` 的暴露面等价，不含用户信息。

## 已知限制

1. **无 threaded IRQ**。`IrqAction` 只有 `Handled`/`NotHandled`；`provider::IrqOp`
   有 `request_threaded_irq`，但 `DeviceServices` 与 `DeviceCtx` 两层都没有对应
   入口。Linux `devm_request_threaded_irq` **禁止机械映射**。
2. **无 MSI-X per-vector 暴露**。`provider::MsiResource` 有分配接口，`DeviceCtx`
   没有方法；当前只有 virtio-net 在 x86_64 通过 transport 内部的 config+queue
   双向量使用（例外 E-01）。三层 handler 签名并存（`Fn(usize)` /
   `Fn(usize)->IrqEvent` / `Fn()->IrqAction`）是这个缺口的症状。
3. **无 runtime PM / suspend-resume**。`DriverOps` 只有 `probe`/`remove`，
   **没有** `suspend`/`resume` 预留位。
4. **无热插拔再枚举入口**。`add_device` 是增量的，但没有旧模型
   `DeviceManager::rescan()` 的等价物；bus instance 级别的 rescan 也未实现。
5. **`DeviceSnapshot::kind` 恒为 `None`**（`domain/mod.rs` 的 `snapshot_of`
   里注释"until a class view exists"）。后果是 `/proc/devices` 丢失了按设备类
   分组与状态列——相对旧模型是观测面回退。
6. **观察者契约靠文档**（T-11），类型上不阻止重入。
7. **`select_batch` 每轮深拷贝 `DeviceDescriptor`**，`modalias` 每次重算。
   probe 非热路径，已记录为可接受代价（`docs/driver-framework-review.md` §8.3 类别 B）。
8. **`PlatformBus` 的参考实现住在本 crate**（`src/bus.rs` 的 `platform` 模块）。
   它应该在独立的 `bus-platform` crate 里；只要枚举只由组合根做，暂时留在这里。

## 审计清单

改动本 crate 时逐项检查：

- [ ] `grep -rni 'virtio|nvme|ahci|mmc|i2c|usb' src/` 是否仍只命中注释？（L0-a）
- [ ] 新增的域状态是否落在 `DomainInner` 的**一把锁**下？有没有引入第二把锁
      从而重新需要锁序文档？
- [ ] 新增的 check-then-act 序列是否整体在锁内？（父设备校验、批选择、独占
      移除声明都是这类）
- [ ] 有没有在持锁期间调用驱动代码、做 MMIO、分配内存、或调用观察者？
- [ ] 新的 teardown 步骤是否遵守 `quiesce → 撤发布 → 释 IRQ → drop transport
      → unmap` 的顺序？
- [ ] 新增的 guard 是否 `take()` 掉 `ReleaseHook`（保证至多释放一次）？
- [ ] 新的 `DeviceState` 迁移是否在所有并发入口（probe 提交、remove 三相、
      wake）下都被考虑？是否需要 bump `generation`？
- [ ] 新的失败模式是 `Deferred` 还是 `Fatal`？分类错了会导致设备永久 parked
      或过早 `Unclaimed`。
- [ ] 新的 `unsafe` 是否有紧邻的 `SAFETY:` 说明，并写清哪个 safe 入口保证不变量？
- [ ] 新的公开 API 是否有 rustdoc（含 `# Errors` / `# Panics` / `# Safety`）？
- [ ] `domain/tests.rs` 是否覆盖了新的状态迁移？特别注意：涉及关联/ID 的修复，
      回归测试必须覆盖那条关联链本身（`docs/driver-framework-review.md` §8.4 教训 2）。
- [ ] 是否引入了对某个设备家族的命名？如果是，应该改成 `ExtensionService`。
