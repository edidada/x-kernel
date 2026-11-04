# RNG 模型评审：问题台账与分批处理计划

状态：用户要求集中整理、分批解决；本页确定处理顺序和边界，不替代未完成的设计验收。
基线：`0d76bc6e6`；依据 [横向评审](rng-lifecycle-model-comparison.md) 与
[Linux 源码对照](rng-linux-lifecycle-reference.md)。

## 1. 总原则

- 已实现框架先修正、review、验证，再整理为可独立理解的提交与 PR。
- 行为修复、机制精简和纯命名整理分别提交，避免重命名掩盖生命周期变化。
- 以下是问题处理批次，不等于已有开发 tickets，也不承诺一批恰好一个 PR。
- 整个平台枚举迁移仍暂停；缺失的 virtio-mmio 驱动列为明确的后续结构性工作，
  先补设计，不因本页自动恢复默认配置切换、block/console 或网络迁移。
- 每批完成后核对文档、调用方、测试及独立 review，再进入下一批。

## 2. 问题台账

| 编号 | 问题 | 分类 / 处理 |
|---|---|---|
| M-00 | 跨驱动 MatchScore、同分后注册优先 | 已修复：`7f9798126`；保留在待整理 PR 的验收材料中 |
| M-01 | 管理锁之外仍用生命周期 generation 判定 probe 提交失效 | 第一批已实现并验证，见下方验收记录 |
| M-02 | Fatal probe 丢弃具体错误，注册成功但未绑定时缺少原因 | 第二批已实现并验证，见下方验收记录 |
| M-03 | 发现统计只覆盖旧路径却使用总量语义的名称/日志 | 第二批已实现并验证，见下方验收记录 |
| M-04 | remove/Active/online 等名称与实际注册、绑定语义混淆 | 第三批按用户确认名称完成整理；验收见下方 |
| M-05 | attempted 写成永久失败；Removed 只有保留状态；physical_path 被过度类比 DEVPATH | 第三批完成合同修正与空状态清理；与命名分开提交 |
| M-06 | 缺独立的 virtio-mmio transport 驱动与 VirtIO 功能设备层 | 结构性缺口，第四批设计后再开发 |
| M-07 | provider 注册只有 static 发布、没有撤销合同 | 后续 provider 驱动接入前置；当前 RNG 不使用，不扩实现 |
| M-08 | 无驱动定义注销/模块卸载接口 | 当前范围限制，文档明确；不把设备解绑冒充驱动卸载 |

## 3. 第一批：精简 probe 提交模型

目标：同一设备的绑定、解绑、注销由管理锁串行化，不再维护正常路径无法变化的
生命周期版本计数及独立过期结果回收协议。

实施前核对全部写入点；保留对象身份检查、必要状态不变量和明确错误处理。
不能连带删除依赖通知 generation、驱动候选注册变化检查或每设备管理锁。
把直接篡改内部版本的测试替换为真实入口下的串行化/拒绝重入证据。

验收：probe 与解绑/注销交错、同设备重入拒绝、失败回滚、remove 先于 Data
释放、重绑创建新实例；model UT 及相关 host 管理锁测试。

## 4. 第二批：失败诊断与发现统计

目标：框架报告哪个设备、哪个候选驱动、为什么 probe 失败；区分正常拒绝与
实际初始化故障，避免所有情况都输出 error。

保留 add_device 成功与绑定成功分离。优先统一失败日志，不立即新增错误历史表。
发现统计应明确旧路径口径；如果增加新路径统计，分别表达注册与绑定结果，不能
把 add_device 返回 Ok 当作 activated。旧路径/新路径的总数不得混算或重复计数。

验收：没有匹配、候选拒绝、真实失败、Deferred、失败后下一候选成功、注册成功
但全部 probe 失败；日志具有设备与驱动来源，原有绑定结果不变。

## 5. 第三批：命名与公开合同

已确认并实施：`remove_device → unregister_device`、`Active → Bound`；明确
`is_online` 是注册存活快照而非硬件可用性，避免换名后又混淆注销开始/完成时点。
`PlatformModel` 的名字体现 RNG 过渡注册责任，`root()` 体现初始化副作用。

保留 `RngBinding`、`DeviceCore/DeviceRef/DeviceAccess`、`model/os-api/kdriver`
的已有职责划分。`DeviceDomain` 先补类型文档，不为相似词强行做全树重命名。
修正 attempted 的轮次语义和 physical_path 的实际范围；审核后才决定是否移除
无生产状态转换的 Removed。状态删除单独提交，不混进纯机械改名。

验收：无旧 API 遗漏消费者，rustdoc/设计/测试名称同步；目标构建和现有 UT
通过。不引入虚假的完整 sysfs DEVPATH 或新的路径唯一性规则。

## 6. 第四批：补齐 virtio-mmio 驱动职责

**缺的不是 MMIO 寄存器实现。** vendor 已有 `MmioTransport`，当前 RNG 已经
使用它。缺的是作为平台驱动注册、绑定平台设备、创建并管理 VirtIO 功能子设备
的 transport 层，不能通过复制 vendor 代码或改一个类型名补齐。

目标职责图：

```text
PlatformBus / compatible="virtio,mmio"
  → VirtioMmioDriver（候选名）
      → 注册 VirtioDevice（描述/运行能力的具体类型待定）
          → VirtioBus 按 device/vendor 匹配
              → VirtioRngDriver（候选名）
                  → hwrng 注册与 RNG 请求生命周期
```

transport 负责物理资源、协议识别及 child 注册/退出；功能驱动负责 RNG 队列、
请求、能力发布与关闭。IRQ/DMA 的交接必须保留现有完整回调 drain 与 reset
确认合同，不把“父对象仍有引用”当作硬件仍可用的证明。

先完成 [能力交接设计](platform-migration-transport-handoff.md) 的未决项：

1. child 同步 probe 前如何取得运行能力，且不查询尚未提交的 parent Data。
2. vendor 按值 Transport 的独占所有权、构造失败回滚及功能重绑。
3. child 解绑保留 parent；parent 解绑必须先注销 child，旧引用不能重获能力。
4. 旧发现入口与新 transport 注册的唯一归属，避免同一 MMIO 双重注册/映射。
5. 独立测试入口及配置：模块接线证明不冒充整个普通平台入口已切换。

这些收口并验收设计后，才能拆开发 tickets，按“必要承载机制 → transport/总线
接线 → RNG 迁入 → 组合验收”实施。是否需要新的通用 model 承载点仍须证明，
不默认批准动态 Any 表、全局设备表或新的 vendor 改动。

验收至少包含：空 slot、未知功能未绑定、同步 child probe、部分失败、两实例
隔离、功能与父 transport 分别重绑、旧句柄关闭、IRQ 通知尾部/共享 peer。
若切换完整普通平台发现入口，则必须恢复先前设计门槛：block、串口接管、根盘
启动及旧 PCI 子树兼容，不能用 RNG-only 通过宣称完整迁移完成。

## 7. 当前下一步

前三批 M-01～M-05 已整理，下一步 M-06 的 transport 分层设计收口，不直接
开始平台迁移开发。M-07/M-08 先明确支持范围，不新增无消费者的完整动态框架。

## 8. M-01 验收记录

- 基线 `51fdb4760`；删除生命周期 generation、过期提交回收及人工修改版本的
  测试；保留管理锁、身份/Probing/空绑定不变量、依赖及注册表变化检查。
- 新 model 测试在 probe 内调用真实 bind/unbind/remove 入口，验证同任务重入
  被拒绝而成功 Data 正常提交；随后解绑/重绑/注销验证 remove-before-drop。
- 新 host 测试阻塞 probe，确认另一任务 unbind 等待；开 gate 后无条件 join，
  再重绑/清理/断言。等待任务的异常停滞交由外部 watchdog 判失败，不提前返回。
- 原有 host probe/注销交错、并行设备推进、初始化失败回滚及清理一次性测试
  也在本轮执行；规范和需求两路评审完成，收尾问题已修正并复核。

验证配置为 AArch64 canonical qemu_defconfig，先 `make defconfig`；独立 worktree
与 disk，SMP4，无网络/vsock，非性能测量。执行：

```sh
timeout 600 make unittest UNITTEST_CRATE=driver_model,kdriver XKMAKE_ARGS='--no-net --no-vsock'
timeout 600 make clippy UNITTEST=y
```

最终 162 passed / 0 failed / 5 ignored，coverage 持久化并导出成功；UT clippy、
修改文件 nightly-2026-03-08 rustfmt 和 diff 检查通过。忽略项是单独 opt-in 的
真实 MMIO 专项及 MMIO 启动断言，不计为本轮通过；不声称重跑完整硬件验收。

首轮运行暴露上一轮顺序匹配修改遗漏的 hwrng 重绑测试：它仍假定后注册 B
自动赢过 A。测试现让 A 显式拒绝后续 probe，由 B 按顺序接管；不改变生产匹配
策略，单独提交该测试修正。初次失败保留在 `.scratch/driver-model-review-m01/ut.log`，
最终结果在同目录 `ut-final.log`，lint 在 `clippy-ut.log`，配置在 `config.log`。

## 9. M-02/M-03 验收记录

- 基线 `d6732f7be`；probe 失败记录设备身份、路径、驱动及具体错误。
  Unsupported 记 debug，其余 RuntimeError 记 warn；不新增错误历史表，
  不改变注册成功、候选推进、Deferred 或显式重试的语义。
- 新测试覆盖五类 probe 错误后的注册保留和显式重试；已有无匹配、Deferred、
  失败后下一候选成功用例一并回归。
- EnumerationContext 公开统计改为 legacy 前缀，日志明确排除 model，区分
  旧设备活跃数增长和描述符激活数；未绑定列表明确包含重排队、未认领及失败。
  新测试验证 model 暂存注册不增加旧统计，取消暂存释放范围预留。
- 规范与需求两路独立 review 无阻断；修改文件格式及 diff 检查通过。

沿用第一批 AArch64 canonical 配置、独立 disk、SMP4、无网络/vsock，运行同一
model+kdriver QEMU UT 命令。WARN 与 DEBUG 两种配置均为 164 passed / 0 failed /
5 ignored，coverage 成功导出。忽略项仍是 opt-in MMIO 专项，不作为硬件验收。
实际 guest 日志断言确认四类实际错误在两种配置各输出两次，Unsupported 只在
DEBUG 出现两次；发现日志明确标注 legacy，并实际出现活跃增长 5、描述符激活 4
的不同结果，未将二者混称总激活数。已恢复 canonical WARN 配置，UT clippy 通过。

证据在 `.scratch/driver-model-review-m02/`：`ut.log`、`ut-debug.log`、
`log-assertions.log`、`clippy-ut.log`，以及配置生成日志和两份配置快照。
本批不恢复平台迁移，不新增 model 总量统计或硬件 transport 分层。

## 10. M-04/M-05 验收记录

基线 `926cef526`。用户确认命名后实施：

- 新模型 `remove_device` 改为 `unregister_device`，驱动 remove 回调不改；
  `DeviceState::Active` 改为 `Bound`，状态字符串同步为 `bound`。
- `PlatformModel` 改为 `PlatformRngRegistration`，`root()` 改为
  `ensure_root_initialized()`；保留模块路径及专项测试过滤入口，避免影响验收脚本。
- 旧 kdevice 的 Active/Removed 及管理 API 不改。procfs 的 model 行显示 bound，
  legacy 行仍显示 active；formatter 用例直接使用新状态的 as_str 验证区别。
- 明确 is_online 在注销开始、remove 回调前关闭，普通解绑不关闭；attempted
  由显式 bind 清空，不是永久黑名单；路径只是总线位置，不承诺 DEVPATH/唯一性。
- 补充 DeviceDomain 职责，保留已暂停的历史设计稿，不将旧提案改写成现行合同。
- 全树核对确认新模型 Removed 无写入、无有效生产状态转换，单独删除变体和
  冗余判断；注销仍直接删除 entry，旧引用离线，重复注销不重复清理。

验证沿用 AArch64 canonical defconfig、独立 disk、SMP4、无网络/vsock；扩大到
`UNITTEST_CRATE=driver_model,kdriver,procfs`。首轮 213 passed / 0 failed / 6 ignored，
coverage 导出成功；补齐 formatter 用例后再次运行同组回归，仍为 213/0/6，
coverage 再次导出成功。六项忽略仍是 opt-in
MMIO 验收，不将其计为通过。现有绑定/解绑/重绑/注销测试还增加 bound 字符串和
解绑后来源保持 online 的断言。规范与需求两路 review 无剩余阻断，修改文件
nightly-2026-03-08 rustfmt、diff 检查及 `make clippy UNITTEST=y` 通过。
日志在 `.scratch/driver-model-review-m04/`，最终回归为 `ut-final.log`。
