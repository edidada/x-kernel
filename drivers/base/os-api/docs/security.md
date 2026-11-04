# driver-os-api — 安全与可靠性

## IRQ callback boundary

IrqHandler and IrqThreadHandler are Send + Sync contracts; sharing their Arc
does not quiesce the device or extend DMA validity. The argument is the resolved
virtual IRQ, not the firmware hardware number. IrqEvent preserves source bits
and threaded intent without allocation. Ordinary callbacks cannot sleep.
The xkernel ordinary adapter preserves handled/source information but has no
paired IRQ thread; wake intent alone cannot create one. Threaded registration
remains unsupported in that provider. Callback sharing does not change guard
ownership, hardware-stop obligations or synchronous release requirements.

## MMIO mapping ownership

The unsafe MmioOp implementation guarantees non-null live mapping extents.
IoMap::map_with retains the original mapping address, extent and provider;
drop consumes its acquisition once. Raw MmioMapping is copyable metadata,
not an additional owner: unmap requires proof of the original live acquisition.
Providers must synchronize their mapping references, and users must stop
register/device access before cleanup. Safe accessors check bounds and alignment
before volatile operations. IoMap does not serialize device register protocols.
The raw-pointer escape does not extend the mapping lifetime.

Host mapping/registry allocation may still fail. Direct provider retention
avoids a separate Arc release-hook allocation but does not make mapping itself
allocation-free. Hook-based unsafe construction retains its existing caller
obligations. Tests cover all register widths, per-acquisition release, rejection
without release, framework LIFO cleanup and native overlapping mapping references.

## CPU-only aligned storage

`src/memory.rs` adds an unsafe `CpuMemoryOp` implementation boundary and two
raw-memory operations: initialization of the complete allocation, and creation
of its exclusive initialized byte view. The safe `CpuBuffer` API depends on the
provider's size/alignment/uniqueness guarantees. Its lifetime retains the
provider; drop returns the original pointer and layout once, after all views
end. Zero-size requests are rejected before provider entry. Acquisition errors
expose neither a handle nor uninitialized memory.

The capability must not establish DMA mappings or change cache attributes.
It grants no bus address or device access guarantee; those remain with the
separate `DmaOp` protocol. The handle has no `Send`/`Sync` overrides. Tests
cover initialization, alignment, matching release, empty requests and injected
allocation failure; real memory exhaustion is not induced.

## IRQ teardown boundary

An IRQ release hook returning successfully authorizes callback-owned state to
be reclaimed. It must detach the original action and drain existing callbacks,
not log a rejected release and continue freeing resources. Callers must meet
the provider's context contract and not hold locks required by handlers. Shared
line ownership does not transfer with one action. Request failures must leave
no newly registered callback; successful requests may invoke it before return.

## Held DMA provider boundary

The consolidated raw DmaOp retains the existing mainline contract. Its
copyable allocation/mapping records are release tuples, not independent owners.
CoherentMem's private owner enum selects exactly one release path and retains
the original tuple without reconstructing it from a translated device address.
Typed pointer views do not initialize arbitrary Rust T values or grant CPU
access while hardware owns the memory.

This migration does not resolve the raw contract's safe-method/raw-pointer
boundary or the static HAL's reconstructed release tokens. Those pre-existing
limitations require a separately scoped contract change. Likewise the existing
raw adapter's zero-bus streaming shortcut is not proof that address zero is
invalid; no claim is made here that this release policy has been repaired.
The stronger DeviceDma safety requirements below are not weakened to match it.

Implementing `DeviceDma` is unsafe: successful coherent allocations must be
valid, exclusive, aligned, zeroed and coherent throughout their lifetime, and
all results must obey the captured device view. Guards retain the exact native
release token and provider; service replacement must not redirect cleanup.
Streaming errors must not copy unused receive data into the original buffer.

The unsafe streaming caller supplies initialized bytes and keeps its buffer
live and exclusive until guard cleanup completes, stopping device access before
Drop. Providers must not copy back uninitialized receive bounce bytes, even
when hardware wrote only part of the buffer. They must preserve original bytes
not written by hardware, rather than clearing those bytes on copy-back. Thus
rollback before hardware publication cannot overwrite earlier receive buffers.
A mapping guard does not
own the original buffer or stop hardware. Release contexts are provider-defined;
Send + Sync does not promise IRQ-safe or non-sleeping operations. The mandatory
private release hook cannot be extracted from or cloned through the guard.

## 范围与信任

覆盖 mmio、irq、dma、clock、error 和 ReleaseHook。宿主 provider 是可信实现，
描述符和驱动请求不证明硬件有效。此 crate 无用户内存入口，不注册 IRQ 或分配
真实资源；对应的验证、访问授权、内核转换由宿主承担。

## Unsafe 边界

unsafe 集中在 mmio.rs，沿用提取前的实现：
- IoMap::new 要求调用方提供至少 region.size 字节、在句柄生命周期内有效的映射，
  并拥有与释放 hook 配对的权限。
- access_ptr 在访问前检查 offset 范围与实际地址对齐，再计算指针。
- read8/16/32/64 与 write8/16/32/64 使用 volatile 操作，并保留 acquire/release fence。
- fence 不替代设备特定屏障、DMA 一致性或并发寄存器访问协议。

错位、越界访问属于驱动错误，会 panic；真实物理地址合法性和映射重叠不由
IoMap 判断。提取不扩展 unsafe 范围或放宽构造条件。

## 所有权与清理

每个 guard 的 hook 至多调用一次，但同一 Arc hook 被多个 guard 复用仍可能重复
释放同一底层资源；宿主必须保证一个实际获取对应一个释放责任。
驱动必须在 Drop 前停止设备、等待在途使用并按依赖顺序销毁 guard。
DevIrq 的 hook 必须完成宿主要求的 handler 撤销与同步，单独丢弃描述值不完成清理。

CoherentMem 的裸指针不提供安全解引用或 DMA 同步保证；分配、地址视图、
缓存维护与设备停止由宿主/驱动协作完成。
EnabledClock Drop 平衡一次成功 enable；enable 失败由 provider 自行清理其部分
副作用，再由句柄回滚 prepare。keep_running 会有意保持 enabled 状态，不自动释放。

## IRQ 与错误边界

DMA 窗口构造检查 CPU 与 bus 两侧区间末端，不使用可能截断的有符号差转 u64。
完整区间转换拒绝零长度、窗口越界、长度加法溢出、非连续 mask 及跨 mask 上界。
窗口字段私有，转换中的 bus 加法由构造不变量与区间检查共同保证不溢出。
这只验证地址范围，不能证明物理内存归属、缓存协议或设备已停机。
宿主分配后若验证失败，必须释放该分配；不得只验证起点后把整个缓冲区交给设备。

irq-types 的 IrqDomainId 不证明域存在，宿主必须验证并执行正确映射。
IRQ handler 只用于普通设备 IRQ，不能睡眠或被当作 pseudo-NMI 回调。
默认恒等 map_irq 不适用于非恒等中断域。新模型没有 threaded IRQ 入口。

ResError 不携带 DeviceId、依赖等待或 probe 策略；model 显式转换错误分类。
不允许为了错误转换而让本 crate 反向依赖 model、旧 device_res 或内核实现。
