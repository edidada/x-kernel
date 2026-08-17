# kalloc — 安全与可靠性分析

## 信任模型

`kalloc` 是内核全局 allocation 边界。它信任以下前置条件：

- boot/平台内存管理提供的 region 已映射、可写、独占且不会与现有页 buddy region
  重叠；
- `GlobalAlloc`、`GlobalAllocator::dealloc()`、页 allocation API 和 `GlobalPage`
  的调用者使用原始 allocation 的地址、页数、`Layout` 与 `UsageKind`，并且只释放一次；
- `alloc-engine` 的 buddy、TLSF、slab 与其 backing memory contract 成立；
- DMA caller 所需的物理地址限制与平台 `v2p` 转换语义一致；
- 启用 slab IPI provider 时，`kipi` 的 publish-before-notify 和 CPU 已启动状态契约成立；
- 启用 `dice`/`tee` C shims 时，C caller 遵守对应 ABI 的 pointer、长度与生命周期契约。

本 crate 负责把页、字节 allocation、PCP、slab object storage、usage accounting 和
可选 tracking 组合为一致的全局 owner；它不验证恶意或错误内核 caller 伪造的 allocation
来源。

## 外部边界 / 攻击面

普通 Rust allocation API 不直接解析用户内存、文件系统、网络、IPC、MMIO/PIO、DMA
buffer、bootloader、firmware 或设备输入。它接受的直接输入是内核 `Layout`、页数、
alignment、虚拟地址和初始化 region。

存在两类可选边界：

- `dice` 或 `tee` 构建中的 `ffi.rs` 导出 C allocation / memory-operation shims。C ABI
  caller 提供的 pointer、长度和重叠关系不能由 Rust 类型系统证明，shims 只实施其
  rustdoc 所列的基本检查；
- `tracking` 在内核内记录 allocation metadata 和 backtrace。它不接受用户 payload，
  但会在内核内存中保存可用于诊断的地址相关信息。

`GlobalPage::from_raw()`、allocator 初始化和 slab backing span 都是 raw ownership
边界；用户指针或未验证设备地址不得直接作为这些接口的输入。

## unsafe 代码清单

| 路径 | 依赖的不变量 | 保护方式 |
|---|---|---|
| `unsafe impl GlobalAlloc for GlobalAllocator` | allocation/deallocation pointer 与 `Layout` 配对，内部 route 返回有效非空对象 | 统一进入 `GlobalAllocator`；失败交给 `handle_alloc_error`，不伪造 pointer |
| 页 allocator 初始化、扩展与 direct page route | region 已映射、独占、页对齐且不与外部 owner 重叠 | `BuddyPageAllocator` region 检查、初始化状态断言与调用者 contract |
| `GlobalPage::{from_raw,fill,as_slice,as_slice_mut}` | `GlobalPage` 是唯一 owner，页范围仍有效 | unsafe constructor 的 ownership contract；其余方法由 `self` / `&mut self` 约束 |
| non-slab byte allocator deallocation | object 属于对应 Buddy/TLSF heap，`Layout` 与 allocation 匹配 | `GlobalAllocator` 以同一 layout 选择 route；下层 `ByteAllocator` contract |
| slab span prepare/publish 与 central deallocation | backing span 几何、class、对象来源和 live 生命周期正确 | `SizeClass` 路由、`PreparedSlab` token、class lock 与 `SlabCache` unsafe contract |
| task magazine / hardirq reserve transfer | object/batch 只在匹配 class 的 central/local storage 间移动 | task magazine current-CPU pin；hardirq 独立 reserve；NMI 不支持 |
| frozen magazine remote drain | target 对同一 generation 已发布 frozen，session 尚未 end | pending-active 只能由目标 task 完成；IPI 只完成 pending-idle；raw access 的 `SAFETY:` 约束 |
| `ffi.rs` C shims（启用时） | C pointer、长度、allocation provenance 和 overlap 满足 ABI contract | null/长度检查与 metadata header；其余范围/所有权由 C caller 保证 |

## 内存安全不变量

1. 页 buddy 管理的 region 在使用期间保持映射、可写、独占且不重叠；page allocation
   只能由对应页接口归还。
2. PCP 中的 1–4 页块只属于当前 CPU 的 page set 或全局 buddy 之一。它的 mutation
   必须发生在 local IRQ 屏蔽的上下文，不能与同 CPU hardirq 并发访问。
3. Buddy/TLSF 的 heap region 在 byte allocator 持有期间仍由它管理；大 layout 直接
   page allocation，不得误交给 byte heap。
4. `GlobalPage` 的 `Drop` 恰好归还它持有的连续页；从 raw 地址重建时不能同时存在第二个
   owner。
5. slab span 只属于一个 `SizeClass` 的 central `SlabCache`。`Partial` / `Full` span
   含 live object，只有 `Empty` span 可 detach 并归还 backing pages。
6. task magazine 与 hardirq reserve 中的 object 仍属于对应 central class。remote drain
   只能在同一 generation 的 frozen acknowledgement 后访问 task magazine。
7. usage accounting 是诊断性 bytes snapshot，不表示实际 object/page ownership，也不
   能作为 free、reclaim 或 allocator correctness 的依据。
8. tracking metadata 不能递归触发自身 allocation bookkeeping；C shim metadata header
   只由同一 shim 的 paired free 读取和释放。

## 线程安全

全局页 buddy 由 `SpinNoIrq` 保护。Buddy/TLSF 配置的 byte allocator 也由独立的
`SpinNoIrq` 保护；PCP 依靠 current CPU 加 local IRQ 屏蔽，不被其它 CPU 直接访问。

runtime singleton 的 usage accounting 以每 CPU atomic signed delta 更新，`usages()`
读取的是可并发变化的诊断 aggregate。显式构造的测试 allocator 维护独立锁保护的 usage
table。可选 tracking 用独立 `SpinNoIrq` state 与 per-CPU recursion guard，避免
`GlobalAlloc` bookkeeping 重入。

slab 每个 `SizeClass` 有独立 `SpinNoIrq<KmemCache>`。页申请、span 初始化和 detached
page 归还不在 class lock 内；task/softirq magazine 只在 current-CPU pin 下访问，normal
hardirq 只访问对应 reserve。全 CPU flush 的 IPI handler 只推进原子状态，发起 task 在
冻结确认后处理对象 batch。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 无效、重叠或提前失效的初始化 region | 高 | 平台/内核 caller 传入错误虚拟范围 | 页 buddy region 验证；调用者必须维持映射与独占性 |
| T-02 | 错误 `Layout`、页数或 route 的 deallocation | 高 | double free、跨 allocator free、错误 alignment | `GlobalAlloc` / page API contract；下层 allocator contract；不把 untrusted pointer 传入 raw owner API |
| T-03 | PCP 在未屏蔽 IRQ 时被重入 | 高 | 当前 CPU task 与 hardirq 同时修改 page set | `pcp` internal API 要求 local IRQ disabled；公开 page route 在访问前建立 guard |
| T-04 | 重复或错误的 `GlobalPage` owner | 高 | `from_raw` 与现有 owner 重叠，或范围已归还 | unsafe constructor contract；`Drop` 只由唯一 owner 执行 |
| T-05 | DMA page 物理范围不满足设备限制 | 中 | 平台地址翻译错误或 allocation 超过 DMA32 | `alloc_dma_pages` 以 `v2p` 检查范围并归还不合格页；driver 仍须遵守设备 DMA contract |
| T-06 | slab backing span 或对象 freelist 被错误修改 | 高 | 错误 class、span 对齐、double free 或跨 class free | canonical `SizeClass`、page state 与 `SlabCache` unsafe contract |
| T-07 | IPI 与 mutable task magazine borrow 并发 | 高 | target CPU 正在本地访问 magazine 时开始 reclaim | pending-active 不可由 IPI 完成；target 在 borrow 结束后发布 frozen |
| T-08 | 延迟 slab IPI 重新冻结已结束 session | 中 | IPI 晚于 unfreeze 到达 | generation-tagged state；handler 不从 idle 创建 frozen state |
| T-09 | C shim 使用无效 pointer 或长度 | 高 | C caller 违反 malloc/free/memcpy ABI | shim 基本参数检查；完整对象范围和 ownership 仍是调用方 contract |
| T-10 | tracking bookkeeping 递归进入 global allocator | 中 | 记录 allocation 时再次 allocation | per-CPU recursion guard 跳过嵌套 bookkeeping |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 应对措施 |
|---|---|---|---|---|---|
| F-01 | 页 buddy 无可用范围 | region 耗尽或碎片化 | page allocation 返回 `NoMemory` | 请求方处理失败或 global allocation 终止 | 返回 `AllocError::NoMemory`；slab 可先 reclaim 空 span |
| F-02 | Buddy/TLSF heap 无空间 | byte heap 耗尽 | 小 layout allocation 失败 | byte allocation 无法继续 | 从页 buddy 扩展 heap 后重试；若页不足则返回错误 |
| F-03 | slab central cache 无对象 | 所有 span 为 Full 或尚未 refill | class allocation miss | 请求 backing pages | class lock 外 prepare/publish 新 span；页不足时尝试 empty-span reclaim |
| F-04 | hardirq reserve 耗尽 | hardirq burst 超过 class reserve | hardirq allocation 返回 `NoMemory` | 调用方执行既有失败路径 | 不在 hardirq 执行 page work 或 task magazine 操作 |
| F-05 | shrink 无法归还页 | object 仍 live，或 span 不是 `Empty` | span 保留 | 内存压力持续 | 只回收可证明为空的 span，避免 use-after-free |
| F-06 | target CPU 长时间未 frozen | target IRQ 延迟或尚未结束 local magazine borrow | slab reclaim 停顿 | OOM reclaim 无法推进 | 等待 matching generation acknowledgement；不在未 frozen 状态 remote drain |
| F-07 | tracking state 不可用或被禁用 | feature/运行时开关关闭 | 无 allocation records | 仅诊断能力缺失 | allocation 本身继续按正常路径执行 |

## 故障管理

显式 allocation API 通过 `AllocError` 返回失败；`GlobalAlloc::alloc` 记录失败后调用
`handle_alloc_error`。`alloc_dma_pages()` 对不满足 DMA32 的页先归还再返回 `NoMemory`。
`GlobalPage` 构造函数将下层错误映射为 `KResult`。

slab backing page allocation 首次返回 `NoMemory` 时，先 flush 已启动 CPU 的 magazine、
drain reserve 并 detach `Empty` span，然后重试一次。`shrink()` 仅是显式 slab reclaim
入口，不能替代整个系统的 memory-pressure policy。buddy/TLSF 配置不存在 central slab
span，因此 `shrink()` 返回零。

## 隐私分析

`kalloc` 不处理用户 payload、身份标识或网络/文件内容。free object 的首 word 可能被
slab freelist 覆写，重新 allocation 前是否清零由调用者定义。

启用 `tracking` 时，crate 在内核内存中保留 layout、generation 和 backtrace 等诊断
metadata；这些可包含内核地址信息。crate 不自行向用户空间或外部遥测导出它们，但暴露
这些记录的上层接口必须按内核调试信息处理。

## 已知限制

- slab flush 只覆盖启动时已建立的 present CPU；没有 CPU offline/online、NUMA、
  remote-free 或 memory cgroup lifecycle。
- PCP 不参与 slab shrink，也没有全系统 memory-pressure flush protocol。
- slab NMI allocation/deallocation 不受支持；hardirq reserve 耗尽时返回 `NoMemory`。
- 不提供 production double-free、错误 `Layout` 或跨 allocator free 检测。
- usage accounting 是 best-effort snapshot；它不等价于页、span 或对象的线性化所有权。
- C shims 仅在相应 feature 下构建，且不把 C caller 的 pointer/length contract 转化为
  可验证的 Rust ownership。

## 审计清单

- allocator 初始化与 `add_memory` 是否只接收已映射、独占、无重叠的 region？
- 每条 page、byte、DMA 和 `GlobalPage` 路径是否以原始 address、count、`Layout` 和
  `UsageKind` 配对释放？
- PCP 调用点是否始终在 local IRQ 屏蔽下，且没有把 PCP 误当成 slab magazine？
- Buddy/TLSF heap 扩展是否仍由页 buddy 提供有效 region，且没有泄漏其 ownership？
- slab layout 是否只由 `SizeClass::from_layout()` 选择，span 是否只在 `Empty` 时 detach？
- class lock 是否不包围页 allocation、span 初始化或 detached page 归还？
- hardirq 是否只使用 reserve，NMI contract 是否在所有调用点保持明确？
- remote magazine access 是否只发生在 matching frozen generation 后，且 session end 前
  已完成 drain？
- 新的 CPU lifecycle、NUMA 或 pressure mechanism 是否拥有独立 owner，而非把
  present-CPU snapshot 或 usage accounting 当作这些机制的替代？
- 启用 C shim 或 tracking 时，是否审计了 ABI pointer/length contract、tracking recursion
  guard 与调试 metadata 的访问边界？
