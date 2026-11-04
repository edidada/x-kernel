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
| M-01 | 管理锁之外仍用生命周期 generation 判定 probe 提交失效 | 当前机制精简，第一批 |
| M-02 | Fatal probe 丢弃具体错误，注册成功但未绑定时缺少原因 | 当前诊断缺口，第二批 |
| M-03 | 发现统计只覆盖旧路径却使用总量语义的名称/日志 | 当前诊断口径，第二批 |
| M-04 | remove/Active/online 等名称与实际注册、绑定语义混淆 | 命名整理，第三批；具体名字先确认 |
| M-05 | attempted 写成永久失败；Removed 只有保留状态；physical_path 被过度类比 DEVPATH | 合同与状态整理，第三批；删除状态前核对消费者 |
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

优先确认：`remove_device → unregister_device`、`Active → Bound`；明确
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

先执行第一批 M-01，完成后自审与验收并单独提交。M-06 不遗忘，但不抢跑未完成
的所有权设计。M-07/M-08 先明确支持范围，不新增无消费者的完整动态框架。
