# memspace — 设计文档
## 定位

`memspace` 是 X-Kernel 的地址空间 owner。它维护 `MmSpace`、`VmAreaSet`、
`VmArea`、页表根和 VMA-side runtime dispatch。

它不拥有 file-backed cached content，也不拥有 anonymous page content。
内容对象分别由 `kvfs::AddressSpace` 和 `mm/anon` 管理；`memspace` 只保存
VMA metadata、运行时执行引用和页表状态。

## 背景

当前模型显式区分三类职责：

- `VmAreaSet`：地址空间里的 VMA metadata 容器。
- `VmArea`：单个映射实例的 range、permission、backing、file metadata 和
  inheritance metadata。
- `VmRuntimeRef` / `VmRuntimeOps`：VMA-side map/unmap/protect/fault/fork/mremap
  执行入口，角色接近 Linux `vm_operations_struct`。
- `MmUserHandle` / `MmPin`：地址空间生命周期 capability。`MmUserHandle`
  对应 Linux `mm_users`，表示仍有 runtime 或临时路径 active 使用用户映射；
  `MmPin` 预留 Linux `mm_count` 风格对象 pin，只保证 `MmSpace` 对象和稳定
  identity 可观察，不延长用户映射生命周期。

Linux 对应关系：

- `MmSpace` 对应 `mm_struct`。
- `VmArea` 对应 `vm_area_struct`。
- `VmRuntimeOps` 对应 VMA 侧 `vm_operations_struct`。
- `VmBackingInfo` 描述 VMA 指向的 file/anon/linear backing object。

## 范围

- `src/aspace.rs`
- `src/vma.rs`
- `src/fault.rs`
- `src/backend/*`
- `src/iomap.rs`
- `src/vmalloc.rs`
- `src/kernel_stack.rs`

## 内核栈后端（vmalloc / kernel_stack）

`src/kernel_stack.rs` 是 `kstack` 的 memspace 侧后端：boot 时通过
`kstack::register_ops` 注入，`ktask` 只依赖 `kstack`，不依赖 memspace，
避免 `memspace -> ksync -> ktask` 依赖环。

- `VMAP_STACK=n`：后端为全局堆（`alloc`/`dealloc`），无 guard page。
- `VMAP_STACK=y`：后端为 `src/vmalloc.rs`。它在内核地址空间 iomap 之后的
  专用区间（`VMALLOC_BASE` 起）内找空闲 VA，逐页映射物理页，并在栈底留
  一个 unmapped guard page；栈向下增长，越界写触发 page fault。

`vmalloc` 的目标是 Linux `vmalloc` 的通用等价物（virtually-contiguous
kernel memory），内核栈是其首个客户。结构与 Linux 对齐：

- VA 分配由 `vmalloc.rs` 内部的独立区间表（`BTreeMap`，对应 Linux
  `vmap_area` 树）管理，不进入地址空间 VMA 树；分配区间为
  `[VMALLOC_BASE, VMALLOC_END)`，每架构显式定义（x86_64/loongarch64
  上界收敛到 kimage 起点，避免与内核映像重叠）。
- 页表更新在单次 `PageTableMut` 事务内完成，TLB flush 批量合并
  （对应 Linux `map_vm_area` + 一次 flush）。
- `free` 可从 hardirq/softirq 上下文调用：异常上下文内的释放进入无锁
  侵入链表，由 `system_wq` 在任务上下文真正拆除映射（对应 Linux
  `vfree_deferred`）。NMI 上下文不支持。

锁序固定为 `VMALLOC` → `KERNEL_ASPACE`（均 `SpinNoIrq`），无反向路径。
持锁期间不发起同步 TLB shootdown：页表提交用 `finish_no_flush()`，flush
统一在锁释放后进行：本地调用 `karch::flush_tlb`，远端调用
`kipi::tlb::flush_all_cpus`（只刷远端 CPU）。远端广播受
`ALL_CPUS_STARTED` 门控；门控关闭期间的 free 不拆 PTE、不释放物理页，而是
进入 deferred 队列，待门控打开后再执行完整的本地与远端 flush，避免 target
CPU 等锁（中断关）时无法应答 IPI 的互相死锁。PTE 拆除后，释放的区间在
flush 完成前仍保留在 interval table，随后才重新获取 `VMALLOC` 锁将其释放；
因此并发分配不会在任一 CPU 仍持有旧 TLB 翻译时复用该 VA。deferred 释放走
无锁链表，IRQ 上下文不会取 `VMALLOC` 锁。
死锁复盘见 `docs/issues/2026-08-17-tlb-shootdown-deadlock.md`。

## 架构

```text
MmSpace
  -> MmUserLifetime
  -> PageTable
  -> MmCpuResidency
  -> VmAreaSet
       -> VmArea
            -> VmBackingInfo
            -> FileMappingInfo
            -> VmRuntimeRef
                 -> Linear
                 -> AnonShared
                 -> AnonPrivate
                 -> FileShared
                 -> FilePrivate

object owner
  -> vmobj::ObjectInvalidateRequest
  -> MmSpace invalidate sink
  -> VMA lookup and runtime unmap
```

## 调用约束 / 执行上下文

- `MmSpace` 由外层 `Mutex` 或等价独占访问保护。
- map/unmap/protect/fault/fork/mremap 路径可能分配内存，运行于可睡眠上下文。
- 不适用于中断上下文。
- 调用者必须在 syscall、loader 或内核 VM 边界完成用户输入校验。

## 算法流程

### VMA 查询

1. `MmSpace::find_vma()` 在 `VmAreaSet` 中定位 VMA。
2. `VmArea::backing()` 返回 `VmBackingInfo`。
3. `VmArea::page_offset()` 表示 Linux `vm_pgoff` 风格的 backing page offset。
4. `VmArea::file_mapping()` 返回 file-backed introspection metadata。
5. `VmArea::runtime()` 只在 `memspace` 内部用于执行 map/unmap/fault 操作。

### Futex backing 解析

`MmSpace::resolve_futex_backing()` 是 non-private futex key 的 MM 边界：

- private VMA 返回 `(mm_id, virtual_address)`；
- shared anon/file VMA 返回
  `(VmObjectId, backing_page_index, byte_offset_in_page)`；
- shared offset 通过 `VmArea::backing_offset_for()` 计算，因此 VMA split、trim、
  不同 virtual mapping 和非零 file offset 都保持同一 object-relative identity。

该 API 只读取 VMA metadata，不触发缺页。调用者仍需使用 kuaccess 完成实际
用户字访问与 fault-in。

### VMA 插入

1. 调用方构造 `VmArea` 和对应 `VmRuntimeRef`。
2. `MmSpace::map_runtime_vma()` 校验 VMA 范围和 runtime backing 一致性。
3. runtime 执行 eager map 或注册 lazy fault 入口。
4. `VmAreaSet::try_insert()` 拒绝重叠 VMA。
5. runtime 注册 file/anon object view 时使用当前 `mm_id` 和 `InvalidateHandle`。

### VMA 变形

- `VmAreaSet::unmap()` 支持 exact removal、front/back trim 和 middle split。
- `VmAreaSet::protect()` 支持 partial range split，只更新 current permission。
- `VmAreaSet::merge_adjacent_where()` 只合并语义等价且 runtime kind 一致的相邻 VMA。
- file-backed `FileMappingInfo.offset` 和 `page_offset` 在 split/trim 时同步更新。

### mremap relocation

`mremap` move-style relocation is not modeled as ordinary `unmap`.

```text
source snapshot
  -> map_relocated_snapshot()
  -> move_pages()
  -> drop_mapping_metadata()
```

The three ownership layers are explicit:

- `VmArea` metadata: this virtual range has a mapping role;
- present PTE residency: this virtual page currently points at a frame;
- backing object ownership: anonymous state is owned by anon objects; the
  file-backed source and its object identity are owned by the inode
  `kvfs::AddressSpace`.

Relocation may change the first two layers without destroying the third.

### Page fault

```text
FaultInput
  -> MmSpace::handle_fault_input()
  -> VMA lookup
  -> permission check
  -> FaultContext with VMA-derived offsets
  -> VmRuntimeRef::handle_fault()
  -> PageFaultOutcome
```

`FaultOutcome` 保留 Linux 用户可见 fault class：

- resolved；
- retry / COW retry；
- unmapped；
- access denied；
- file-range `BusError`；
- out of memory；
- no progress / failed。

### Object-driven invalidate

```text
kvfs::AddressSpace / anon object
  -> vmobj::ObjectInvalidateRequest
  -> InvalidateHandle
  -> MmSpace invalidate sink
  -> drain_pending_invalidations()
  -> apply_invalidate_request()
  -> runtime.unmap()
```

`MmSpace` 收到 object-side invalidate 后，重新按当前 VMA metadata 检查：

- object id 是否匹配；
- shared/private view kind 是否匹配；
- object offset 与 VMA offset 是否仍一致；
- request range 是否仍覆盖当前 VMA overlap。

匹配后只 zap present PTE，保留 VMA metadata。下一次 fault 重新按当前 object 状态裁决。

若单个 request 应用失败，`MmSpace` 将其放回队列尾部并结束本轮 drain，避免同一边界无限重试。

### Private anonymous discard / COW

- `AnonymousPrivate` 和 `FilePrivate` 的 post-write private pages 由
  `AnonPrivateObject` 持有。
- `MADV_DONTNEED` 通过 runtime 将 VMA range 转成 object range，再由
  private-anon helper detach object page slots、unmap present PTE、完成 TLB
  finalization 后释放 frame。
- COW write fault 使用 object slot 比较和 `PageTableMut::replace_if_same()` 的
  PTE snapshot 比较。任一侧发生变化都返回 retry-class outcome。

### `msync`

`MmSpace::msync_range()` owns Linux-style VMA range traversal for `msync`.
It records unmapped holes as `NoMemory` while still dispatching later mapped
VMAs in the range.

Only shared file runtimes perform writeback. Anonymous, linear, and private
file runtimes return not-applicable through the runtime hook, so private COW
pages are never written back to the source file.

`MS_INVALIDATE` returns not supported in the current model because locked VMA
and invalidate semantics are not represented yet.

### Protection Changes

`MmSpace::protect_mapping_range()` separates VMA permission state from present
PTE flags. The requested flags are recorded in `VmAreaSet::protect()`, while
`VmRuntimeOps::on_protect()` returns the effective PTE flags to install for
already-present mappings.

This allows runtimes such as shared file mappings to keep writable VMAs backed
by write-protected PTEs until the first write fault performs runtime-specific
dirty tracking.

### Fork

1. 新建 child `MmSpace`。
2. 复制每个 `VmArea` metadata。
3. shared runtime 共享 object identity。
4. private runtime 通过 `AnonPrivateObject` lineage 和页表写保护形成 COW。
5. 父/子页表修改完成 TLB finalization 后，才提交 child object page state。

## 并发模型

- `MmSpace` 由外层锁串行化。
- `MmUserHandle` 的 user-count 状态转换不依赖 `MmSpace` 主锁；从裸
  `Arc<Mutex<MmSpace>>` mint handle 时会短暂读取 mm-owned lifetime，最后一个
  user 释放时会获取 `MmSpace` 主锁并执行 `clear()`。
- `MmUserHandle::clone_user_unless_zero()` 与最后一个 user release 通过同一个原子
  状态转换互斥；一旦进入 torn-down 状态，后续 `MmPin` 不能再升级为 active user。
- `MmCpuResidency` 由 `MmSpace` 拥有，用于记录“哪些 CPU 可能还保留该地址空间
  的 TLB 状态”。调度切换路径在切入新 mm 前先 set CPU，硬件切换完成后再从旧 mm
  clear CPU，保证 shootdown 最多过度命中、不会漏掉目标 CPU。
- `InvalidateSink` 使用 `SpinNoIrq` 保护短队列操作，不在该锁内执行 runtime
  unmap 或获取 `MmSpace` 主锁。
- `PageTable::modify()` 提供页表独占修改 guard。
- `VmAreaSet` 不暴露独立可变共享访问。
- file/anon object 的内部并发由对应 object crate 自己维护。

## 设计决策

1. `VmArea` 保存 Linux 风格 VMA metadata。
   原因：split/protect/unmap/fork/mremap 都需要稳定 metadata，而不是从 runtime
   私有状态反推。

2. `VmRuntimeRef` 只表示 VMA-side execution reference。
   原因：runtime 可以执行 fault 和 PTE 操作，但不能成为 file cache 或 anon page
   owner。

3. `VmBackingInfo` 显式区分 `Linear`、`AnonymousShared`、`AnonymousPrivate`、
   `FileShared`、`FilePrivate`。
   原因：file-private mapping 必须同时保留 file source object、anon result
   object 和 anon lineage。

4. `FaultInput` 与 `FaultContext` 分离。
   原因：trap/user runtime 输入只包含 CPU 报告的 fault facts；VMA-derived
   object offsets 必须在 VMA lookup 后附加。

5. Object-driven invalidate 使用 `vmobj` request。
   原因：file-backed 和 anonymous object 都需要同一套 object -> VMA view 语言。

6. `msync` 使用 runtime hook 而不是 syscall 反查 pagecache。
   原因：`MmSpace` owns VMA traversal, while `filemap` owns file-backed runtime
   source state. This keeps syscall ABI, VMA dispatch, and content-object
   writeback in separate owner crates.

7. relocation source retirement is separate from ordinary runtime unmap.
   原因：ordinary private/file-private unmap may detach backing object pages;
   successful relocation only retires the old virtual role.

## Drop / 资源释放

- `MmSpace::clear()` 通过 runtime unmap 清理全部 VMA 的 present PTE。
- `PageTableMut::finish()` 是释放 detached frames 前的 TLB finalization boundary。
- `VmAreaSet` 清空后，VMA metadata 和 runtime references 一起释放。
- File/anon object view guards 随 runtime/object 引用释放并注销 view。
