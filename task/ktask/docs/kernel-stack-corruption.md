# ktask — 内核栈溢出导致偶发 EL1 EXECUTE panic（已定位 + 已修复）

> 状态：**根因已确认、已修复、已验证**。
> 影响版本：`main` @ `f046b3b0`（`!535`）及之前；平台 `aarch64-qemu-virt`（及其它架构同样存在），`UNITTEST=y`。
> 最后更新：2026-07。

## 1. 现象

在 aarch64（`aarch64-qemu-virt`，`NR_CPUS=4`，SMP）上执行 `make UNITTEST=y run` 全量内核单元测试时，偶发内核 panic：

```
panicked at arch/kcpu/src/aarch64/excp.rs:61:5:
Unhandled EL1 Page Fault @ 0xffff...., ESR=0x8600000f (EXECUTE)
```

- 默认构建下：单线程串行需反复重跑才出现；**16 路并行施压约 1/15 命中**。
- 与具体被测 crate 无关（最后打印的 kprocess 用例只是恰好排在最后），属系统级问题。
- 故障地址落在 `__llvm_prf_cnts`（RW、不可执行）；寄存器转储里 `x19–x30` 大量是数据区指针/近 0——典型的"`ret` 载入垃圾返回地址后跳进数据段"。

## 2. 根因（已确认）

**`ktask::TaskInner::new_user` 让调用方自选内核栈大小；`util/unittest_support::run_in_user_task` 传了硬编码的 `64 * 1024`（64KB）——比真实用户任务使用的 `KERNEL_STACK_SIZE`（256KB）小 4 倍。** unittest-user 任务跑 **fork 测试**时，fork 的地址空间复制路径把这个 64KB 内核栈用爆：

```
task_entry
 → unittest_support::run_in_user_task
 → kprocess::tests::test_oom_score_..._fork_inherited
 → Thread::prepare_process_fork
 → fork_process_runtime
 → prepare_fork_address_space
 → MmSpace::try_clone()          ← 深拷贝地址空间，撑爆 64KB 内核栈
```

溢出写过栈底，溅射进**相邻**任务的内核栈 / `TaskContext`。相邻任务随后返回时 `ldp x29,x30,[sp]` 载入被腐蚀帧里的垃圾 → `ret` 到不可执行地址 → EL1 EXECUTE panic。

这解释了此前全部"怪象"：
| 观察 | 解释 |
|---|---|
| 崩溃在测试体、与并发度强相关 | fork 触发窗口随调度并发度升高 |
| canary 受害者自己 `sp` 正常、栈底却被零写 | 它是**相邻受害者**；被 fork 任务的溢出帧（零值局部）写进栈底 |
| 把 `TASK_STACK_SIZE` 调到 1MB 不缓解 | 64KB 是 `run_in_user_task` **硬编码**，与 `TASK_STACK_SIZE` 配置无关 |
| 非 TaskContext UAF、非退出路径 bug | 对——是 fork 任务**自己**的栈溢出溅射到相邻任务 |

## 3. 如何定位（guard page）

给每个 `TaskStack` 栈底加一页 **guard page**（unmap 栈底页，写即数据访问缺页，带写者 PC）。它一次定位到两处：

1. **测试体写者**：fork 路径中一个定时器中断进来，异常向量压栈时 `sp` 已落到 guard 页 → 写穿 guard。调用链符号化即上述 `task_entry → … → prepare_process_fork`（确凿）。
2. **boot 时另一处越界**：`devfs::capture_firmware_dtb_snapshot` 用固定 `DTB_CAPTURE_SIZE = 2MB` 从 `dtb_vaddr` `copy_nonoverlapping`，远超实际 DTB，2MB 越界读进内核堆（栈所在）。

## 4. 修复（三项）

### 4.1 根因：用户任务内核栈大小改为内核职责

`new_user` **去掉 `stack_size` 参数**，内部统一用 `KERNEL_STACK_SIZE`。用户任务能执行任意 syscall（尤其 fork，要复制整个地址空间），内核栈尺寸是内核的职责、不该由调用方决定——既修了 64KB，又杜绝此类复发。同步更新全部调用方：`posix/process/{init_process.rs,runtime.rs}`、`process/kprocess/src/tests.rs`（6 处）、`util/unittest_support/src/lib.rs`。

### 4.2 安全网：所有内核栈加 guard page（始终启用）

- `task/ktask/src/task.rs`：`TaskStack::alloc` 分配时把栈底页在内核页表里 unmap（`aarch64` 下），`Drop` 时按保存的 PTE 重映射。
- `arch/karch/src/aarch64/{mmu.rs,mod.rs}`：新增原生内核页表遍历 `unmap_kernel_4k` / `remap_kernel_4k`（4 级 walk + Inner-Shareable `tlbi` 广播，所有核同步失效）。
- 任何未来的栈溢出 / 越界写都会在**写的那条指令**处即时数据访问缺页，给出明确的写者 PC 与 backtrace，而非静默腐蚀相邻内存。

### 4.3 独立 bug：DTB 快照按实际大小读取

`fs/filesystems/devfs/src/nodes/dtb.rs::capture_snapshot` 改为先从 FDT 头读实际 `totalsize`（offset 4，big-endian，校验 magic `0xd00d_feed` 并夹在 firmware 报告的 region 内），只拷那么多字节；头部无效则跳过（不再盲拷 2MB）。这也让 guard page 能从 boot 起就启用（capture 不再越界触 guard）。

## 5. 验证

- 串行：**1317 passed, 0 failed**（0 guard 缺页、0 崩溃）。
- 并行施压 12 路（正是此前确定性触发 bug 的同等负载）：**12/12 全过，0 崩溃**。
- guard page 始终启用下 boot 全程无误触（证明 4.3 彻底）。

## 6. 复现（历史，已不复现）

修复前的默认构建（概率性，并行施压易触发）：

```bash
cp platforms/aarch64-qemu-virt/defconfig .config && make defconfig
make UNITTEST=y build
for w in $(seq 0 15); do
  make UNITTEST=y justrun HOSTFWD_PORT=$((5800+w)) VSOCK_CID=$((400+w)) QEMU_ARGS="-snapshot" >/tmp/r_$w.log 2>&1 &
done; wait; grep -lE "Unhandled EL1 Page Fault" /tmp/r_*.log
```

## 7. 环境

- openkylin/x-kernel，`main` @ `f046b3b0`（`!535`）。
- 平台 `aarch64-qemu-virt`，`NR_CPUS=4`，SMP，EEVDF，`UNITTEST=y`（启用 coverage）。
- QEMU：`-m 1g -smp 4 -cpu cortex-a76 -machine virt,gic-version=3`。
