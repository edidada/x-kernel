# drv-clk — 安全与可靠性分析

## Scope

`drivers/drv/clk/src/{lib,fixed_clock}.rs`（126 行）。

## 信任模型

| 输入 | 来源 | 信任级别 |
|---|---|---|
| `phandle`、`clock-frequency`、`clock-output-names`、`#clock-cells` | **设备树**（经 `driver-init::build_descriptor` → `FirmwareNodeInfo`） | **半可信**。数值本身不参与地址计算，但 `phandle` 会成为 provider 的**身份键**，`clock-frequency` 会成为下游所有消费者的时钟频率 |
| `kclk` 的 registry | `dev.env().provide::<driver_host::ClkProvider>`（经 `Host`） | 可信 |

关键区别：本 crate 的固件输入**不产生内存安全后果**（没有地址、没有长度、
没有指针），但会产生**语义后果**——一个错误的 `clock-frequency` 会让所有
消费者以错误的波特率/分频运行。

## 外部边界 / 攻击面

| 边界 | 是否触及 |
|---|---|
| 用户内存 / 用户指针 | 否 |
| MMIO / PIO | **否**——`fixed-clock` 是纯 DT 描述，没有寄存器 |
| DMA | 否 |
| 中断 | 否 |
| **设备树** | **是**（唯一攻击面） |
| 文件系统 / 网络 | 否 |

## unsafe 代码清单

无 `unsafe`。有一处**刻意的资源泄漏**（`Box::leak`），见 `design.md`；
它不是 unsafe，但是一个安全相关的生命周期决定，因此在下面 FMEA 里跟踪。

## 内存安全不变量

1. **`FixedClockLookup` 被 leak 成 `&'static`，且它持有的 `Arc<ClkCore>` 永不
   释放。** 这是 `kclk` boot-static registry 的要求。后果：不存在
   "registry 里的 lookup 指向已释放的 ClkCore" 的悬垂窗口——这是本 crate
   最重要的内存安全性质，它靠"永不释放"而不是靠引用计数协调来保证。
2. **`dev.env().provide::<ClkProvider>` 是 probe 的最后一步**，其后没有可失败操作。
   因此不存在"provider 已注册但 probe 返回 Err"的状态——那种状态会让
   消费者被唤醒后又发现 provider 不可用。
3. **`name: &'static str` 借自 DT blob**，因此 DT blob 必须常驻。这与
   `driver-init` 的不变量 2 是同一个假设。

## 线程安全

- `FixedClockDriver` 是零大小类型，`Send + Sync`。
- `FixedClockData` 只含 `Arc<ClkCore>`；`ClkCore` 自身的并发由 `kclk` 负责
  （`core/kclk/src/invariants.rs` 有 964 行专门描述时钟树的锁与不变量）。
- `FixedClockLookup` 被 `&'static` 共享给 registry，因此必须 `Sync`：
  它的两个字段都不可变（`ProviderId` 是 `Copy`，`Arc<ClkCore>` 只 clone），
  满足。
- 本 crate 无锁、无原子。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 畸形 DT 给出 `clock-frequency = 0` 或极大值，下游消费者（UART 波特率、定时器分频）算出错误的时序 | 中 | 被篡改或错误的 DT | **未实现校验**。`FixedRate::new(frequency as u64)` 接受任意值。**残余风险**：错误的时钟频率会让 UART 波特率偏移（表现为乱码）、让依赖该时钟的定时器计时错误。加固建议：拒绝 `frequency == 0`（`ClkBuilder` 或本 crate 的 probe 里），并对 `u32 → u64` 的上界做合理性检查（例如 > 10 GHz 视为固件错误） |
| T-02 | 两个 DT 节点声明**同一个 phandle**，第二个 provider 注册覆盖或冲突 | 中 | 畸形 DT | **已实现**：`Host`/`ClkRegistry` 记录已服务的 phandle，重复注册返回 `RuntimeError::AlreadyExists` → probe `Fatal`；key 与 lookup 自称 id 不一致返回 `Invalid`。消费者不会拿到被覆盖的时钟 |
| T-03 | DT 有大量 `fixed-clock` 节点，每个 leak 一个 lookup + 一棵 `ClkCore` | 低 | 畸形/恶意 DT | **未实现上限**。每个泄漏是几十字节，DT 节点数受 `driver-init` 的 u32 id 空间限制。**残余风险**：启动期内存放大，无运行期后果 |
| T-04 | 移除 fixed-clock 设备后 provider 仍可用，消费者继续用一个"设备已消失"的时钟 | 中 | 热插拔 / 测试场景 | **刻意为之**（boot-static 语义），但**没有可见性**：`remove()` 空操作，不记日志，`kclk` registry 不变。见 F-03 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | probe 返回 `Fatal(Invalid)` | 描述符没有 `FirmwareNodeInfo`，或缺 `clock_frequency` / `phandle` | 该节点不发布 provider | 依赖它的消费者永久 deferred，直到 `driver-base::finish_probe_phase` 在启动末尾报告并降级为 `Unclaimed`（实测日志：`device 70 gave up waiting on dependency clk:66 (provider never registered)`） | 3 | 三个字段分别检查，都是 `Fatal` 而不是 `Deferred`——因为"节点自称 fixed-clock 却没有 frequency"是固件错误，等下去不会变好 |
| F-02 | `ClkBuilder::build()` 失败 | `kclk` 内部校验（名字、ops 组合） | probe `Fatal(Io)` + `log::warn` | 同 F-01 | 3 | `inspect_err` 记录名字，便于定位是哪个节点 |
| F-03 | 设备被移除 | 域 remove | `remove()` 空操作；`Data` drop 减一个 `Arc`，但 lookup 仍持强引用 | **provider 仍然注册、仍然可用**，`kclk` registry 不变 | 3 | 刻意的 boot-static 语义。**残余风险**：状态与直觉不符且无日志。加固建议：`remove()` 里 `log::warn!` 说明"clock provider 不随设备移除而注销"，把隐式语义变成可见行为 |
| F-04 | provider 注册失败（重复 phandle / key 不一致 / host 不支持该家族） | 固件或 board 缺陷 | probe `Fatal`，设备 `Unclaimed` | 该 provider 的消费者保持 deferred，启动末尾报告 | 3 | 注册是 probe 最后一步，失败不会留下"已注册但 probe 失败"的半状态；失败原因按 `RuntimeError` 分类 |
| F-05 | wake 钩子未接上，消费者不被唤醒 | 域没有装 services，或 wake 闭包指向别的域 | 消费者永久 parked | 启动末尾 `finish_probe_phase` 兜底报告 | 3 | `Host::install_global()` 把 wake 接成 `global_domain().wake`；`driver-base` 的 `provider_registration_wakes_exactly_its_consumer` 与 driver-host 的 `consumer_defers_until_ccu_publishes_then_activates` 钉住这条链路 |

## 故障管理

- probe 的错误一律 `ProbeError::Fatal`——本 crate **从不返回 `Deferred`**。
  这是正确的：provider 驱动没有自己的依赖（不消费时钟、不需要 IRQ、不需要
  MMIO），所以没有什么可等。
- `dev.provide` 返回 `Result<(), ProbeError>`：T-02 的重复 phandle、key 不一致
  与"host 不支持该 provider 家族"都由它报告（不再是静默 `()`）
  **无法被 probe 感知**。这是当前最主要的错误通道缺口。
- 本 crate 唯一的 panic 是间接的（F-04，在 `driver_host` 里）。

## 隐私分析

无。`clock-output-names` 是 DT 里的硬件描述字符串，不含用户信息。
日志里出现时钟名与频率（`fixed-clock: published 'clk24m' @ 24000000 Hz`）。

## 已知限制

1. **只支持 `#clock-cells = <0>` 的固定时钟**。有 cells 的控制器（需要
   `get_by_name` / 多输出）不在本 crate 范围。
2. **provider 不可注销**（T-04 / F-03）。`kclk::Registry::unregister` 存在但
   无调用者（构建时报 dead_code 警告）。
3. **`clock-frequency` 无合理性校验**（T-01）。
4. **重复 phandle 无检测**（T-02）。
5. **`get_by_name` 恒返回 `None`**，因此按名字查这个 provider 的时钟一定失败。
6. **每个节点泄漏一个 lookup**，无上限（T-03）。

## 审计清单

- [ ] 新增 provider 驱动时，`register_*_provider` 是否是 probe 的**最后一步**？
      （不变量 2：注册之后不能再失败）
- [ ] leak 的对象是否只含不可变字段？（它会被 `&'static` 共享，必须 `Sync`）
- [ ] 固件给的数值是否参与了地址/长度计算？本 crate 没有，但新的 provider
      驱动（例如 clock controller 要映射寄存器）会有——那时 T-01 的分析要重做。
- [ ] probe 返回 `Fatal` 还是 `Deferred`？provider 驱动通常应该是 `Fatal`
      （没有依赖可等）；消费者驱动缺依赖时必须是 `Deferred`。
- [ ] `remove()` 的语义是否与"boot-static"一致？如果不一致（例如新 provider
      支持注销），必须真的注销，否则会出现"设备已移除但 provider 还在"。
- [ ] 是否在 `driver-init::init()` 里加了 `register()` 一行，并在
      `api/kfeat/Cargo.toml` 与 `drivers/Kconfig` 里接了 feature？
      （`scripts/ci/check_feature_matrix.sh` 会自动从 Kconfig 派生一行覆盖）
