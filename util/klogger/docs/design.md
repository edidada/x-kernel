# klogger — 设计文档

## 定位

`klogger` 是 X-Kernel 的日志核心：它实现 `log` crate 的 `Log`，把内核各
模块的 `error!`/`warn!`/`info!`/`debug!`/`trace!` 记录分流到两处消费端：

- **控制台**：按“运行时过滤器”决定这一条是否显示；
- **内核日志环形缓冲区**：按“`log` crate 上限”捕获，供 `syslog(2)` /
  `dmesg` 回读。

控制台输出依赖 `kiface_logging_api` 的 `LogSinkIf`（写字符）与
`LogContextIf`（时间、CPU、任务 ID）provider；provider 由 `kruntime`
在 `init_klogger` 之前链接生效。子系统只依赖 `klogger` 的宏与过滤 API，
不接触具体输出通道。

## 背景

全局单一日志等级无法覆盖真实硬件 bring-up 需求：开启 `debug` 才能看清
某一条栈（如 mmc/IDMAC），但全局 `debug` 会被 IPI、syscall 等热路径日志
淹没；而把全局降到 `warn` 之后，事后又没有任何办法回读启动期间发生过
什么（`syslog` 曾是返回 0 的空桩）。

本模块把两件事拆开：

- **显示什么**：运行时过滤器（默认等级 + 按 target 前缀的覆盖规则）；
- **发生过什么**：内核日志环形缓冲区，捕获所有通过 `log` 上限的记录。

两者共用同一个上限：`log::max_level` 是过滤器的“天花板”，只做便宜的前置
判断；真正的按 target 决策发生在 `Log::log` 内部，因此一条覆盖规则可以
高于默认等级。

## 范围

```
util/klogger/
├── src/
│   └── lib.rs        （过滤器、环形缓冲区、宏、等级解析）
├── ut-tests/
│   └── lib.rs        （过滤器 spec 与缓冲区语义的 guest 用例）
├── docs/
│   ├── design.md
│   └── security.md
└── Cargo.toml
```

## 架构

```
error!/warn!/info!/debug!/trace!
        │  （log crate：先查 log::max_level 天花板）
        ▼
KernelLogger::log
        ├── 1. LineBuf 组装 "<level>[ time cpu:tid target:line] args\n"
        │       └──► LOGBUF（64 KiB 环，覆盖写入） ──► syslog(2)/dmesg
        └── 2. filter_enabled(target, level)
                ├── 是 ──► print_fmt ──► LogSinkIf::write_str
                └── 否 ──► 丢弃（记录已进缓冲区）
```

| 组件 | 职责 |
|------|------|
| `Filter` | 默认等级 + 至多 `MAX_FILTER_RULES` 条 `target=level` 规则；按最长前缀匹配 |
| `FILTER` | `SpinNoIrq` 保护的全局过滤器 |
| `LogBuf` | 定长 64 KiB 字节环，`push` 逐字节覆盖最旧内容，`snapshot` 取最新尾段 |
| `LOGBUF` | `SpinNoIrq` 保护的全局缓冲区 |
| `LineBuf` | 定长 256 B 行缓冲，记录写不进去的部分被截断而不是丢弃整条 |
| `print_fmt` | 控制台输出串行化锁 |

## 调用约束 / 执行上下文

- 任一日志宏可以在任意上下文调用：启动早期（`init_klogger` 之前 `log`
  没有 logger，宏自动成为空操作）、中断上下文、softirq、普通任务上下文。
- `LOGBUF`、`FILTER`、控制台锁都是 `SpinNoIrq`，临界区内不做用户内存
  访问、不睡眠、不分配；`LineBuf` 与环都是定长存储，记录路径没有堆分配。
- `syslog(2)` 的用户拷贝**不在** `klogger` 内发生：`ksyscall` 先在锁外
  分配内核缓冲、调 `logbuf_snapshot` 取快照，释放锁后再写用户空间。
- `set_filter_spec` 允许在运行时（含 `/proc/sys/kernel/printk` 写路径）
  调用；过滤器换入是整体替换，不会半生效。
- `init_klogger` 只在主核启动时调用一次。

## 状态机

过滤器是一份“spec → 生效规则”的单向替换：

```
启动默认（LOG_LEVEL_*，Warn）
    │ set_filter_spec(dt / loglevel= / quiet / debug / LOG_FILTER)
    ▼
{ default, rules[] } ──set_filter_spec(新 spec) 校验通过──► 整体替换
                     └─校验失败──► 保持原状（不改任何一条规则）

set_log_level_filter(level) ≡ 只设 default，清空所有覆盖规则
```

环形缓冲区只有“未满追加 / 已满覆盖最旧字节”两种状态，没有显式清空之外
的状态迁移：`logbuf_clear` 把 `start` 与 `len` 归零。

## 算法流程

### 一条记录的处理

1. `log` crate 用 `log::max_level()` 做天花板判断（spec 的 ceiling：默认
   等级与所有覆盖等级的最大值），低于天花板的记录直接丢弃。
2. `Log::log` 用 `LineBuf` 组装 `<level>[ time cpu:tid target:line] args\n`。
   `<level>` 是 X-Kernel 自己的编号（`1..=5`，error..trace），换行属于记录
   本身 —— 否则 `dmesg` 会把整段日志当成一行。
3. 记录整行 `push` 进 `LOGBUF`（覆盖最旧字节）。
4. 再按 `filter_enabled(target, level)` 决定是否送控制台；被丢弃的记录
   仍然留在缓冲区里，这就是“安静的串口 + 完整的启动日志”可以并存的原因。

### filter spec 的解析

spec 形如 `[<level>] [<target>=<level>]...`，token 用空白或逗号分隔：

1. 含 `=` 的 token 拆成 `target` 与 `level`：`level` 必须是 `0..=5` 或
   `off/error/warn/info/debug/trace`；`target` 去空白、去掉冗余结尾 `*`，
   长度必须为 `1..=MAX_FILTER_TARGET`，规则条数不得超过
   `MAX_FILTER_RULES`；
2. 不含 `=` 的 token 视为默认等级，重复出现时后者覆盖前者；
3. 一条规则都没有、也没有默认等级（空 spec）视为非法；
4. 全部校验通过后才 `*FILTER.lock() = next` 并 `log::set_max_level(ceiling)`；
   任意 token 非法则返回 `FilterSpecError`，旧 spec 完全不受影响。

`filter_spec(buf)` 是它的逆操作：把当前过滤器的规范化文本写进调用者缓冲，
长度不足时返回 `None`（`/proc/sys/kernel/printk` 读路径与启动信息打印共用）。

### 启动等级来源的优先级

优先级由 `kruntime` 决定，`klogger` 只负责校验与生效：
`/chosen/x-kernel,log-level` → 命令行 `loglevel=` / `quiet` / `debug` →
Kconfig `LOG_FILTER` → Kconfig `LOG_LEVEL_*`。非法值只告警并按下一级回退。

## 并发模型

| 共享状态 | 同步方式 | 说明 |
|----------|----------|------|
| 过滤器 `FILTER` | `SpinNoIrq<Filter>` | 读取只发生在日志热路径，规则查询是定长数组扫描 |
| 环形缓冲 `LOGBUF` | `SpinNoIrq<LogBuf>` | 记录写入与快照读取互斥；`push` 逐字节复制，最坏一次记录 256 B |
| 控制台输出 | `print_fmt` 内 `SpinNoIrq<()>` | 保证多核输出不交错 |
| `log` 上限 | `log` crate 的原子 | 与 `FILTER` 分开写：先换过滤器，再换天花板 |

`SpinNoIrq` 关中断，因此日志热路径不能与中断处理竞争同一把锁而死锁；
同时要求临界区内不做任何可能阻塞或重入日志的操作（`LineText` 组装在
加锁前完成，`print_fmt` 的锁不与 `LOGBUF` 嵌套持有）。

## 设计决策

- **显示与记录分离**：控制台过滤器与缓冲区捕获点分开，避免“为了事后
  分析而放开全局等级”导致串口淹没。
- **spec 而非单一等级**：`warn ksyscall=debug` 这类 spec 让 bring-up 只需
  重定向一个栈，且可以运行时修改，不需要重新编译。
- **`log::max_level` 当天花板**：`log` crate 的宏前置判断无法按 target 区分，
  把它降级为“便宜的上限”，真正的判断放进 logger；代价是天花板必须是所有
  覆盖规则的最大值，trace 规则会让所有 trace 记录进入 `log()` 再被丢弃。
- **定长环 + 定长行**：日志路径不能分配，也不能因为一条超长记录丢失前缀；
  单条记录 256 B 之后截断。
- **等级编号沿用 X-Kernel 域**（`0..=5`，越大越啰嗦）：`syslog(2)` 的
  `SYSLOG_ACTION_CONSOLE_*` 故意不实现，因为 Linux 的 `console_loglevel`
  是反向域，混用会误导用户。

## Drop / 资源释放

模块没有需要析构的资源：过滤器与环都是静态存储，`init_klogger` 安装的
logger 是 `&'static`。`logbuf_clear` 是最接近“释放”的操作，它只重置索引，
不清零缓冲区内容（旧字节会在后续覆盖中消失）。