# Linux Threaded IRQ 功能与 X-Kernel 对照

本文基于本地 Linux 源码 `/root/linux-stable` 梳理 threaded IRQ 的使用需求、
generic IRQ core 软件功能、关键数据结构与算法实现，并对照 X-Kernel 当前
`kirq` threaded IRQ foundation 的覆盖情况。

## Linux Threaded IRQ 解决的问题

Threaded IRQ 是 Linux generic IRQ action 的 sleepable bottom half。它不是
softirq、tasklet 或 workqueue 的别名，而是绑定在 `irq_desc` / `irqaction`
生命周期内的 per-action 线程化处理机制。

典型需求场景：

1. **设备中断入口必须很短，但慢路径需要睡眠**
   - primary handler 在 hardirq context 判断是否来自本设备、读取少量状态、ack 或
     mask 设备侧中断；
   - 真正处理在 `thread_fn` 中执行，可以睡眠、拿 mutex、访问 I2C/SPI/regmap、
     等待设备状态机。
   - 典型调用：`drivers/usb/typec/tcpm/fusb302.c` 使用
     `request_threaded_irq(..., NULL, fusb302_irq_intn, IRQF_ONESHOT | IRQF_TRIGGER_LOW, ...)`。

2. **慢速总线或 regmap IRQ controller**
   - PMIC/MFD/regmap 设备的 IRQ 状态寄存器通常在 I2C/SPI 上；
   - hardirq 不适合做多寄存器读写、mask/unmask、nested virq fanout；
   - Linux `drivers/base/regmap/regmap-irq.c` 使用
     `request_threaded_irq(irq, NULL, regmap_irq_thread, irq_flags | IRQF_ONESHOT, ...)`。

3. **GPIO/extcon/Type-C/charger/RTC 等控制面中断**
   - 中断频率不高，但处理常常涉及 debounce、状态同步、通知子系统、访问慢速外设；
   - 大量驱动使用 `devm_request_threaded_irq()`，通常 primary 为 `NULL`，thread
     handler 直接做设备状态同步。

4. **shared IRQ line 上的设备归属判断**
   - shared IRQ 仍需要 primary 阶段判断本次中断是否属于本 action；
   - 只有返回 `IRQ_WAKE_THREAD` 的 action 才唤醒自己的 `thread_fn`；
   - `dev_id` 是 shared line 上释放和识别 action 的 key。
   - 典型调用：`drivers/net/phy/phy.c` 使用
     `request_threaded_irq(..., NULL, phy_interrupt, IRQF_ONESHOT | IRQF_SHARED, ...)`。

5. **level-triggered IRQ 防止线程处理期间重复触发**
   - 对无 primary 的 threaded IRQ，默认 primary 只返回 `IRQ_WAKE_THREAD`；
   - 如果 line 在 thread 完成前重新 unmask，level IRQ 可能因设备仍保持 asserted
     而 storm；
   - `IRQF_ONESHOT` 保证 thread handler 完成前保持 line masked，除非 irqchip
     声明 `IRQCHIP_ONESHOT_SAFE`。

6. **free/synchronize 必须覆盖 hardirq 和 thread_fn**
   - 驱动 teardown 释放 MMIO/DMA/regmap state 前，必须确保旧 hardirq snapshot
     和正在运行的 IRQ thread 都退出；
   - Linux `synchronize_irq()` 等待 hardirq 后继续等待 `threads_active == 0`；
   - `free_irq()` 删除 action 后同步 hardirq/thread，并停止 kthread。

7. **实时内核 / forced IRQ threading**
   - 在 `force_irqthreads()` 场景，Linux 可把原本 hardirq-only action 转换为
     threaded execution；
   - `IRQF_NO_THREAD`、`IRQF_PERCPU`、`IRQF_ONESHOT` 等 flag 控制是否允许 forced
     threading。

高吞吐网络 RX/TX 数据面通常仍使用 NAPI/softirq/poller，而不是简单替换成 threaded
IRQ。Threaded IRQ 更适合控制面、管理中断、慢速设备和需要和 IRQ action teardown
语义绑定的设备慢路径。

## Linux Public API 语义

核心 API 在 `/root/linux-stable/kernel/irq/manage.c`：

```c
int request_threaded_irq(unsigned int irq,
                         irq_handler_t handler,
                         irq_handler_t thread_fn,
                         unsigned long irqflags,
                         const char *devname,
                         void *dev_id);
```

语义：

- `handler` 是 primary hardirq handler；它必须不能睡眠。
- `thread_fn` 是 sleepable IRQ thread handler；`NULL` 表示不创建 IRQ thread。
- `handler == NULL && thread_fn != NULL` 时安装默认 primary handler
  `irq_default_primary_handler()`，它直接返回 `IRQ_WAKE_THREAD`。
- `handler == NULL && thread_fn == NULL` 非法。
- shared IRQ 必须传非空 `dev_id`，否则无法在 `free_irq()` 中定位 action。
- shared IRQ 不能和 `IRQF_NO_AUTOEN` 组合。
- `IRQF_ONESHOT` 表示 hardirq primary 完成后 line 保持 masked，直到相关
  `thread_fn` 完成。
- `IRQF_COND_ONESHOT` 表示当前 action 愿意在 shared line 已经是 ONESHOT 时继承
  ONESHOT。
- `request_irq()` 是 `request_threaded_irq(..., thread_fn = NULL)` 的 wrapper，并
  传入 `IRQF_COND_ONESHOT`。
- `devm_request_threaded_irq()` 在 `kernel/irq/devres.c` 包装
  `request_threaded_irq()`，把 release 交给 devres 生命周期。

返回值在 `/root/linux-stable/include/linux/irqreturn.h`：

```c
IRQ_NONE        // 不是本设备或未处理
IRQ_HANDLED     // primary 已处理完成
IRQ_WAKE_THREAD // primary 认领中断并请求唤醒 action->thread_fn
```

## Linux 关键数据结构

### `struct irqaction`

定义在 `/root/linux-stable/include/linux/interrupt.h`。

关键字段：

- `handler`：primary hardirq handler。
- `thread_fn`：threaded handler。
- `thread`：为该 action 创建的 kthread。
- `secondary`：forced threading 场景下的 secondary action。
- `dev_id` / `percpu_dev_id`：action identity；shared IRQ release 依赖它。
- `next`：shared IRQ action 链表。
- `irq`：Linux IRQ number。
- `flags`：`IRQF_*`，包括 `IRQF_SHARED`、`IRQF_ONESHOT`、`IRQF_NO_THREAD`、
  `IRQF_PERCPU`、`IRQF_NO_AUTOEN` 等。
- `thread_flags`：thread runtime flags，例如 `IRQTF_RUNTHREAD`、`IRQTF_READY`、
  `IRQTF_AFFINITY`、`IRQTF_FORCED_THREAD`、`IRQTF_WARNED`。
- `thread_mask`：shared oneshot 中的 action-local bit。
- `name`：thread/proc/debug 名称。

### `struct irq_desc`

定义在 `/root/linux-stable/include/linux/irqdesc.h`。

threaded IRQ 相关字段：

- `action`：当前 IRQ line 的 action list。
- `lock`：保护 descriptor 与 hardirq/management 状态。
- `request_mutex`：序列化 request/free，避免 free 等待期间重用 action 资源。
- `depth`：disable nesting。
- `threads_oneshot`：shared oneshot active bitset；每个 active threaded action
  对应自己的 `action->thread_mask`。
- `threads_active`：当前已唤醒但尚未完成的 IRQ thread 计数。
- `wait_for_threads`：`synchronize_irq()` 等待 `threads_active == 0` 的 waitqueue。
- `threads_handled` / `threads_handled_last`：threaded handler 参与 spurious
  detection 的统计。
- `istate`：包含 `IRQS_ONESHOT`、`IRQS_INPROGRESS` 等 descriptor internal state。

## Linux 注册算法

主要函数：`request_threaded_irq()` -> `__setup_irq()`。

1. **API 参数检查**
   - `irq == IRQ_NOTCONNECTED` 返回 `-ENOTCONN`。
   - shared IRQ 必须有 `dev_id`。
   - shared IRQ 不允许 `IRQF_NO_AUTOEN`。
   - 非 shared IRQ 不允许 `IRQF_COND_SUSPEND`。
   - `IRQF_NO_SUSPEND` 和 `IRQF_COND_SUSPEND` 不能同时设置。
   - `handler == NULL` 时必须有 `thread_fn`，然后设置
     `handler = irq_default_primary_handler`。

2. **分配并填充 `irqaction`**
   - 保存 `handler`、`thread_fn`、`flags`、`name`、`dev_id`。
   - 执行 irqchip power-management get。

3. **nested threaded IRQ 处理**
   - 若 descriptor 标记 nested thread，则必须有 `thread_fn`；
   - primary 被替换为 `irq_nested_primary_handler()`，真实执行由父 IRQ thread
     上下文驱动。

4. **forced threading 处理**
   - 若 `force_irqthreads()` 且 descriptor 可线程化，并且 action 未声明
     `IRQF_NO_THREAD` / `IRQF_PERCPU` / `IRQF_ONESHOT`，Linux 会强制线程化；
   - 如果 action 同时有 primary 和 `thread_fn`，会创建 `secondary` action；
   - primary 被替换为默认 wake handler，原 primary 移到 `thread_fn`。

5. **创建 IRQ thread**
   - 若 `new->thread_fn && !nested`，调用 `setup_irq_thread()`；
   - kthread 名称为 `irq/<irq>-<name>`，secondary 为 `irq/<irq>-s-<name>`；
   - 保存 `new->thread = get_task_struct(t)`；
   - 初始 bind 到 `cpu_possible_mask`，设置 `IRQTF_AFFINITY`，由 thread 自己在运行后
     根据 effective affinity 调整。

6. **oneshot-safe 与 ONESHOT 规则**
   - 如果 irqchip flags 含 `IRQCHIP_ONESHOT_SAFE`，清除 `IRQF_ONESHOT`，避免多余
     mask/unmask；
   - 如果 `handler == irq_default_primary_handler` 且没有 `IRQF_ONESHOT`，并且
     irqchip 不是 oneshot-safe，则拒绝注册，防止 level-triggered storm；
   - `IRQF_ONESHOT && !thread_fn` 会触发 warning，因为没有 thread_fn 时 ONESHOT
     没有意义。

7. **request/free 序列化**
   - 获取 `desc->request_mutex`；
   - 获取 chip bus lock；
   - 获取 `desc->lock`；
   - 这个顺序同时覆盖慢速 irqchip bus 操作、descriptor action list 更新和 hardirq
     并发。

8. **shared IRQ 兼容性检查**
   - 已有 action 时，新旧 action 必须都设置 `IRQF_SHARED`；
   - trigger type 必须匹配；
   - NMI 不能共享；
   - per-cpu 属性必须一致；
   - ONESHOT 必须一致，或者新 action 用 `IRQF_COND_ONESHOT` 同意继承；
   - 遍历已有 action 的 `thread_mask`，为新 ONESHOT action 找空 bit。

9. **分配 `thread_mask`**
   - `IRQF_ONESHOT` action 获得一个 `1UL << ffz(thread_mask)`；
   - shared line 最多受 `unsigned long` bit 数限制；
   - 非 ONESHOT action 的 `thread_mask = 0`，避免 wake path 判断。

10. **首次 action 激活 line**
    - 首个 action 会 request irqchip resources；
    - 设置 trigger；
    - `irq_activate()`；
    - 设置 `IRQS_ONESHOT`、per-cpu、no-balance、no-debug 等 state；
    - 非 `IRQF_NO_AUTOEN` 时启动 line。

11. **发布 action**
    - 插入到 `desc->action` 链表尾；
    - 更新 PM/proc/debug 状态；
    - 释放锁后唤醒并等待 IRQ thread 设置 `IRQTF_READY`，避免注册返回时 thread 尚未
      进入可接收 wake 的状态。

## Linux Hardirq Dispatch 算法

主要函数：`__handle_irq_event_percpu()` 和 `__irq_wake_thread()`，在
`/root/linux-stable/kernel/irq/handle.c`。

### primary fanout

`__handle_irq_event_percpu(desc)` 遍历 `desc->action`：

1. 对每个 action 调用 `action->handler(irq, action->dev_id)`。
2. 检查 handler 不应打开 local IRQ；如果打开则 warning 并重新关 IRQ。
3. 对返回值做分类：
   - `IRQ_NONE`：本 action 未处理；
   - `IRQ_HANDLED`：本 action hardirq 处理完成；
   - `IRQ_WAKE_THREAD`：必须有 `action->thread_fn`，否则只 warning；
     有 thread_fn 时调用 `__irq_wake_thread(desc, action)`。
4. 所有 action 的返回值 OR 到 descriptor-level return，用于 spurious detection
   和 flow handler 后续处理。

### wake thread

`__irq_wake_thread(desc, action)`：

1. 如果 `action->thread` 正在退出，直接返回。
2. `test_and_set_bit(IRQTF_RUNTHREAD, &action->thread_flags)`：
   - 如果已经置位，说明该 action 已经 pending/running，不重复增加
     `threads_active`；
   - 这把多次 hardirq wake 合并成同一次 thread run，后续 thread 完成后如果又看到
     `IRQTF_RUNTHREAD` 会再跑一轮。
3. `desc->threads_oneshot |= action->thread_mask`：
   - 非 ONESHOT 时 mask 为 0；
   - ONESHOT 时标记该 action 的 thread 尚未完成。
4. `atomic_inc(&desc->threads_active)`。
5. `wake_up_state(action->thread, TASK_INTERRUPTIBLE)`。

这里依赖 `IRQS_INPROGRESS` 与 `desc->lock` 序列化 hardirq wake 和 thread 完成，
避免 thread 比 hardirq 更快完成时过早清掉 `threads_oneshot` 或过早 unmask。

## Linux IRQ Thread Loop

主要函数：`irq_thread()`、`irq_wait_for_interrupt()`、`irq_thread_fn()`、
`irq_finalize_oneshot()`，在 `/root/linux-stable/kernel/irq/manage.c`。

### 等待循环

`irq_thread()` 启动后：

1. 设置 `IRQTF_READY` 并唤醒注册路径等待者。
2. 设置调度策略：
   - 普通 IRQ thread 调用 `sched_set_fifo(current)`；
   - forced secondary 调用 `sched_set_fifo_secondary(current)`。
3. 注册 task exit cleanup `irq_thread_dtor()`。
4. 循环调用 `irq_wait_for_interrupt()`：
   - 设置 `TASK_INTERRUPTIBLE`；
   - 如果 `IRQTF_AFFINITY` 置位，按 IRQ effective affinity 调整 thread CPU mask；
   - 如果 `kthread_should_stop()`，可能最后再处理一次 pending `IRQTF_RUNTHREAD`；
   - 如果 `IRQTF_RUNTHREAD` 置位，清除它并返回运行；
   - 否则 `schedule()`。

### 执行 thread_fn

`irq_thread_fn(desc, action)`：

1. 调用 `action->thread_fn(action->irq, action->dev_id)`。
2. 返回 `IRQ_HANDLED` 时递增 `desc->threads_handled`，供 spurious detection 使用。
3. 调用 `irq_finalize_oneshot(desc, action)`。

forced threading 使用 `irq_forced_thread_fn()` 包装：

- 为非显式 threaded IRQ 保留 hardirq 隐含的 BH/preempt 约束；
- 在非 PREEMPT_RT 配置下临时 disable/enable local IRQ；
- 用 `local_bh_disable()` / `local_bh_enable()` 包住真实 handler。

### 完成与 unmask

`irq_finalize_oneshot(desc, action)`：

1. 非 `IRQS_ONESHOT` 或 forced secondary 直接返回。
2. 获取 chip bus lock 和 `desc->lock`。
3. 如果 `IRQD_IRQ_INPROGRESS` 仍为真，释放锁并自旋重试，等待 hardirq primary
   完成 descriptor in-progress 窗口。
4. 如果 `IRQTF_RUNTHREAD` 又被置位，说明 thread 需要再跑一轮，不清
   `threads_oneshot`。
5. 清除 `desc->threads_oneshot &= ~action->thread_mask`。
6. 若 `threads_oneshot == 0`、IRQ 未 disabled、且 line 当前 masked，则
   `unmask_threaded_irq(desc)`。

thread loop 每次 handler 返回后调用 `wake_threads_waitq(desc)`：

- `atomic_dec_and_test(&desc->threads_active)` 时唤醒 `desc->wait_for_threads`；
- `synchronize_irq()` 和 `free_irq()` 依赖它等待所有 active IRQ threads 完成。

## Linux synchronize/free 语义

### `synchronize_hardirq()`

- 等待 hardirq handler 完成；
- 不等待 threaded handler；
- 返回值可反映是否还有 threaded handler active；
- 可谨慎从 IRQ context 调用。

### `synchronize_irq()`

内部 `__synchronize_irq(desc)`：

1. `synchronize_irqwork(desc)`；
2. `__synchronize_hardirq(desc, true)`；
3. `wait_event(desc->wait_for_threads, !atomic_read(&desc->threads_active))`。

它覆盖 hardirq 与 threaded handler。shutdown/teardown 场景必须使用它或
`free_irq()`，不能只用 `synchronize_hardirq()`。

### `free_irq()`

核心在 `__free_irq(desc, dev_id)`：

1. 禁止 interrupt context 调用。
2. 获取 `desc->request_mutex`、chip bus lock、`desc->lock`。
3. 用 `dev_id` 从 shared action list 中找到目标 action。
4. 从 `desc->action` 链表摘除 action。
5. 若最后一个 action 被移除，先 shutdown line，但完整 deactivate 延后到同步后。
6. 释放 chip bus lock，再调用 `__synchronize_irq(desc)`：
   - 避免 IRQ thread 在 `irq_finalize_oneshot()` 中需要同一 bus lock 时死锁；
   - 仍持有 `request_mutex`，阻止新 request 重用 oneshot bit 或 action lifetime。
7. 若 action 有 thread，`kthread_stop_put(action->thread)`，secondary 也停止。
8. 若最后一个 action，重新获取 bus lock，deactivate irqdomain 并 release resources。
9. 释放 PM/module/proc/debug 资源并返回 devname。

重要点：action 已经从 dispatch list 摘除，但旧 hardirq snapshot 和旧 IRQ thread
仍可能运行，所以 request/free serialization 和 `threads_active` 等待必须覆盖整个
teardown 窗口。

## Linux 软件功能清单

以下功能共同构成 Linux threaded IRQ 的正确性边界：

| 功能项 | Linux 语义 |
| --- | --- |
| Primary hardirq handler | `handler` 在 hardirq context 运行，判断归属、ack/mask、返回 `IRQ_*` |
| Thread handler | `thread_fn` 在线程上下文运行，可睡眠 |
| 默认 primary | `handler == NULL && thread_fn != NULL` 时安装返回 `IRQ_WAKE_THREAD` 的默认 handler |
| 显式 wake return | 只有 `IRQ_WAKE_THREAD` 唤醒 action thread；`IRQ_HANDLED` 不唤醒 |
| Shared fanout | 同一 `irq_desc` action 链表逐个 primary fanout |
| Shared action identity | `dev_id` 定位 action，shared IRQ 必须非空 |
| Shared compatibility | shared、trigger、per-cpu、oneshot 类型必须兼容 |
| Per-action thread | 每个 threaded `irqaction` 有自己的 `task_struct *thread` |
| Wake coalescing | `IRQTF_RUNTHREAD` 避免重复 wake 和重复 `threads_active++` |
| Thread readiness | 注册返回前等待 `IRQTF_READY`，避免早期 wake 丢失 |
| Thread affinity | `IRQTF_AFFINITY` 让 thread 按 IRQ effective affinity 自调度 |
| Thread priority | IRQ thread 设置 FIFO 调度策略 |
| ONESHOT line masking | `IRQF_ONESHOT` thread 完成前保持 line masked |
| Shared oneshot aggregation | `thread_mask` + `threads_oneshot`，全部 active thread 完成后才 unmask |
| Oneshot-safe irqchip | `IRQCHIP_ONESHOT_SAFE` 可跳过 generic oneshot mask dance |
| Nested threaded IRQ | nested irq 使用父 threaded context，不创建普通 hardirq primary |
| Forced threading | `force_irqthreads()` 可线程化普通 action，支持 secondary action |
| synchronize_hardirq | 只等 hardirq，不等 thread |
| synchronize_irq | 等 hardirq 和 `threads_active == 0` |
| free_irq | 摘除 action、mask/shutdown、同步 hardirq/thread、stop kthread、释放资源 |
| Spurious accounting | hardirq return 和 `threads_handled` 参与 spurious detection |
| Diagnostic | `IRQ_WAKE_THREAD` 无 `thread_fn` 时 warning；handler 打开 IRQ 时 warning |
| Devres wrapper | `devm_request_threaded_irq()` 绑定 device-managed release |

## X-Kernel 当前实现对照

当前 X-Kernel 相关文件：

- `arch/kirq/src/model/desc.rs`
- `arch/kirq/src/runtime/action.rs`
- `arch/kirq/src/runtime/dispatch.rs`
- `arch/kirq/src/runtime/thread.rs`
- `arch/kirq/src/runtime/state.rs`
- `arch/kirq/src/runtime/manager.rs`
- `task/ktask/src/irq_thread.rs`
- `task/ktask/src/irq_wait.rs`

### 已覆盖的 foundation

| Linux 功能项 | X-Kernel 当前状态 |
| --- | --- |
| Primary hardirq handler | 已有。`IrqAction::primary` 在 hardirq dispatch snapshot 中运行 |
| Thread handler | 已有。`IrqThreadSlot` 通过 `IrqThreadTaskIf` 创建 action-owned task |
| 显式 wake return | 已有。`IrqEvent::WAKE_THREAD` / `wake_thread_from_sources()` 对应 `IRQ_WAKE_THREAD` |
| `HANDLED` 不唤醒 thread | 已有。`IrqActionReturn::from_event()` 只按 `event.wake_thread()` 产生 `WakeThread` |
| Thread wake coalescing | 部分已有。`pending` bool 合并多次 wake，`sources` OR 聚合 |
| hardirq 不执行 thread_fn | 已有。dispatch 只调用 `thread.wake()` |
| wait hardirq snapshot | 已有。`in_flight` + completion 支撑 `wait_for_virq_in_flight()` |
| synchronize 等 thread idle | 已有。`wait_for_virq_idle()` 先等 hardirq，再等 `thread.wait_idle()` |
| free stop thread | 已有。`try_free_irq_inner()` action removal 后 stop/wait thread exited |
| action lifecycle owner | 已有。thread slot 挂在 `IrqAction` 内，不暴露给 driver |
| scheduler dependency boundary | 已有。`kirq` 只依赖 `IrqThreadTaskIf` / `IrqSyncWaitIf` provider |
| 默认 primary wake handler | 已有。`register_threaded_default()` / `try_register_threaded_default_with_options()` 安装返回 `IrqEvent::WAKE_THREAD` 的默认 primary，并强制 generic ONESHOT |
| ONESHOT line masking | 部分已有。non-shared threaded action wake 前 mask line，thread idle 后按 disable depth 决定是否 unmask；mask/unmask 与 enable/disable/free 控制路径通过 descriptor-local `IrqStateDesc::control -> state -> platform op` 串行化 |
| 当前限制文档化 | 已有。`arch/kirq/docs/design.md` / `security.md` 说明只支持 non-shared regular threaded IRQ |

### 当前缺失或只部分覆盖

| Linux 功能项 | X-Kernel 状态 | 正确性影响 |
| --- | --- | --- |
| Shared threaded IRQ | 未实现 | 不能支持 shared line 上每个 action 独立 wake/thread teardown |
| Shared action threaded token | 未实现 | `free_irq_action()` 当前只针对 shared hardirq action；threaded action 仍是 regular |
| Shared oneshot active mask | 未实现 | 多个 threaded action 时无法保证最后一个 thread 完成后才 unmask |
| Oneshot-safe irqchip 判断 | 未实现 | 当前无法区分 controller 原生 oneshot-safe 与需 generic mask 的 line |
| Thread affinity | 未实现 | IRQ thread 不随 IRQ affinity 调整 |
| Thread priority policy | 未实现 | `ktask::spawn_with_name()` 创建普通 task；没有 Linux FIFO 等价策略 |
| Thread readiness handshake | 部分实现 | X-Kernel spawn 后直接返回 slot；当前 completion wake 应可保留 pending，但没有显式 READY 状态 |
| `synchronize_hardirq()` 单独 API | 未实现 | 当前 `try_synchronize_irq()` 语义接近 Linux `synchronize_irq()` |
| Forced threading | 未实现 | 不支持 RT-style 把 hardirq-only action 强制线程化 |
| Nested threaded IRQ | 未实现 | 不支持 nested irq thread 模型 |
| Spurious accounting with thread results | 未实现 | threaded handler 返回值目前被忽略，不参与诊断 |
| Devres threaded wrapper | 未实现 | `kdriver` 当前只适配 hardirq/shared action，不暴露 threaded IRQ 给 driver |
| `IRQ_WAKE_THREAD` without thread diagnostic | 已有 | 没有 thread slot 的 action 返回 `WAKE_THREAD` 会进入诊断路径，但不会唤醒线程 |

### 当前实现需要重点复核的点

1. **当前 ONESHOT 仍不是完整 Linux `IRQF_ONESHOT`**
   - `THREADED` 只表示 action 带 thread slot；`ONESHOT` 是独立 action flag；
   - non-shared threaded action 已有 descriptor-level line mask/unmask 闭环，且状态变化
     和平台 enable/disable 操作复用 descriptor-local control lock；
   - shared active aggregation、irqchip oneshot-safe 判断尚未实现；
   - shared threaded IRQ 接入前，不能把当前 line-level `ONESHOT` 当作完整 Linux
     shared `IRQF_ONESHOT` 等价语义。

2. **threaded 注册失败后的 descriptor/mapping 残留**
   - `try_register_threaded_with_options()` 先 resolve descriptor，再释放锁 spawn
     thread；
   - 如果 spawn 失败，当前返回 `ThreadUnavailable`，但已经创建的 descriptor/domain
     mapping 可能保留；
   - 这通常不破坏 dispatch 安全，但如果期望 request 失败完全无副作用，需要补
     cleanup 或把文档明确为允许解析副作用。

3. **unpublished thread cleanup 依赖 wait provider**
   - 第二阶段安装 action 失败时会 stop unpublished thread 并 wait stopped；
   - 如果 wait provider 无法等待，只 warning；
   - 因 thread slot 尚未发布给 dispatch，driver state 安全性一般可控，但资源泄漏
     诊断需要后续关注。

4. **`free_irq()` 当前只返回 primary handler**
   - Linux `free_irq()` 返回 devname，action/thread 资源由 core 释放；
   - X-Kernel `free_irq()` 返回 primary `Handler`，thread handler 留在 stopped
     thread slot 内随 action drop；
   - 这符合当前 Rust ownership，但 driver-facing API 若要对齐 Linux，需要明确
     threaded handler 不通过 return 交还。

5. **thread handler 返回值当前被忽略**
   - Linux `IRQ_HANDLED` 会递增 `threads_handled`；
   - X-Kernel `IrqThreadState::run_loop()` 调用 `handler.handle(self.virq)` 后丢弃返回值；
   - 当前没有 spurious accounting 尚可接受，但后续统计/诊断要接入这里。

## 用 Linux 清单校验 X-Kernel 正确性的建议顺序

### Milestone 1：regular threaded IRQ 最小闭环

必须满足：

- `register_threaded()` 只能在空 line 安装一个 regular action；
- primary 在 hardirq context 执行，不能睡眠；
- 只有 `IrqEvent::WAKE_THREAD` / `wake_thread_from_sources()` 唤醒 thread；
- `IrqEvent::HANDLED` 不唤醒 thread；
- hardirq path 不分配、不睡眠、不运行 `thread_fn`；
- 多次 wake 在 thread 未完成前被合并，source bits 不丢；
- `synchronize_irq()` 等 hardirq snapshot 和 active thread idle；
- `free_irq()` 摘除 action 后 mask line，等待 hardirq，stop/wait thread，再清理
  descriptor；
- IRQ thread 不被 `is_in_interrupt_context()` 判断为 hardirq/softirq/BH-disabled。

当前实现基本覆盖该 milestone。

### Milestone 2：driver-facing convenience

对齐 Linux 常用调用面：

- 支持 `handler == None && thread_fn != None` 的默认 primary wake handler。当前
  `register_threaded_default()` 已覆盖 non-shared line，并强制 generic ONESHOT。
- 后续补齐 irqchip/controller `oneshot-safe` 标记后，可在安全 line 上跳过 generic
  mask/unmask。
- 在 `kdriver::resource` / devres 适配层暴露 threaded IRQ registration。

### Milestone 3：完整 ONESHOT

对齐 Linux `IRQF_ONESHOT`：

- descriptor runtime 维护 line-level masked state；non-shared threaded action 已有
  `oneshot_mask_pending` 基础闭环；
- primary 返回 wake 后设置 action-local active bit；
- thread 完成时清除 active bit；
- 只有所有 active bit 清零、line 未显式 disabled、且当前 masked 时才 unmask；
- 与 `disable_irq()`、`disable_irq_nosync()`、`free_irq()`、hardirq in-progress
  状态正确交互；
- 区分 irqchip/controller oneshot-safe。

### Milestone 4：shared threaded IRQ

对齐 Linux shared line：

- shared action 可带自己的 primary、thread_fn、token、thread runtime；
- primary fanout 仍逐 action 判断归属；
- 只有返回 wake 的 action 唤醒自己的 thread；
- `free_irq_action(token)` 摘除指定 shared threaded action，并等待该 action 的旧
  hardirq snapshot 和 IRQ thread；
- shared ONESHOT 需要 per-action active mask 聚合。

### Milestone 5：调度、诊断和 RT 扩展

后续增强：

- IRQ thread affinity 跟随 IRQ affinity；
- IRQ thread priority/policy；
- per-action/per-line threaded IRQ stats；
- spurious accounting 与 thread handler return 合并；
- forced threading / nested threaded IRQ，如有 RT 或 cascaded controller 需求。

## 参考源码索引

Linux:

- `/root/linux-stable/include/linux/irqreturn.h`
- `/root/linux-stable/include/linux/interrupt.h`
- `/root/linux-stable/include/linux/irqdesc.h`
- `/root/linux-stable/kernel/irq/handle.c`
- `/root/linux-stable/kernel/irq/manage.c`
- `/root/linux-stable/kernel/irq/devres.c`
- `/root/linux-stable/kernel/irq/chip.c`
- `/root/linux-stable/kernel/irq/spurious.c`

X-Kernel:

- `arch/kirq/src/model/desc.rs`
- `arch/kirq/src/runtime/action.rs`
- `arch/kirq/src/runtime/dispatch.rs`
- `arch/kirq/src/runtime/thread.rs`
- `arch/kirq/src/runtime/state.rs`
- `arch/kirq/src/runtime/manager.rs`
- `task/ktask/src/irq_thread.rs`
- `task/ktask/src/irq_wait.rs`
