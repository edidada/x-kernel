# device-res-xkernel — 安全与可靠性分析

## 信任模型

```text
device-res provider contract
        │ validated resource descriptors and handler objects
        ▼
device-res-xkernel
        │
        ├── memspace MMIO mapping
        ├── kirq IRQ registration / threaded IRQ / MSI-X
        └── kdma DMA allocation / mapping
```

`device-res-xkernel` 信任：

- `memspace::iomap_device` 拒绝非法 MMIO 物理地址范围；
- `kirq` 校验 IRQ descriptor、管理 action lifecycle、threaded wake/ONESHOT、
  in-flight synchronization 和 teardown；
- `kdma` 返回配对的 coherent/streaming DMA allocation metadata；
- driver remove 路径在 devres cleanup 前已经停止设备 DMA 和中断源。

## Unsafe 边界

### DMA allocation

`alloc_coherent()` 通过 `Layout::from_size_align()` 验证 `DmaSpec` 后调用
`kdma::allocate_dma_memory()`。

不变量：

- layout 非零且对齐合法；
- 返回的 buffer 由 `device_res::DmaCoherent` 或 devres cleanup 独占；
- 释放时使用同一个 `DmaSpec` 重建 layout。

### DMA free

`free_coherent()` 从 `DmaAllocation` 重建 `kdma::DMAInfo` 并调用
`kdma::deallocate_dma_memory()`。

不变量：

- `DmaAllocation` 来自本 provider 的 `alloc_coherent()`；
- 每个 allocation 只释放一次；
- 驱动已停止可能访问该 buffer 的设备 DMA。

### Streaming DMA map/unmap

`map_streaming()` 和 `unmap_streaming()` 根据 `DmaDirection` 调用 `kdma` streaming API。

不变量：

- 调用者提供的 buffer 在 mapping 生命周期内有效；
- `unmap_streaming()` 消费的是同一 provider 返回的 `DmaMapping`；
- direction 与原 mapping 一致。

## 并发与生命周期

- Provider 自身无可变共享状态；全局 provider registry 由 `device-res` 保护。
- IRQ action list、threaded IRQ 状态和 ONESHOT mask/unmask 同步由 `kirq` 保护。
- Devres cleanup 按 `DeviceObject` LIFO 顺序执行；释放 IRQ 时会通过 `kirq` 等待旧
  hardirq snapshot 和 threaded handler 退出。

## 已知限制

- threaded IRQ 当前只支持 non-shared regular action；shared threaded IRQ 仍需在
  `kirq` 中补 per-action thread/token/oneshot active mask。
- irqchip oneshot-safe 能力尚未接入，default-primary threaded IRQ 统一走 generic
  ONESHOT。
- MSI-X 仅在 x86_64 backend 上实现，其他架构返回 `ResError::Unsupported`。
