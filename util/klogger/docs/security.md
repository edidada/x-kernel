# klogger — 安全与可靠性分析

## 信任模型

```
内核模块（可信）──日志宏──────────────► klogger ──► LogSinkIf（控制台）
                                              └──► LOGBUF ──► ksyscall::sys_syslog
启动元数据（引导链，半可信）──filter spec──► set_filter_spec
特权用户（root）──/proc/sys/kernel/printk──► set_filter_spec
特权用户（root / CAP_SYSLOG 语义）──syslog(2)──► logbuf_snapshot
```

`klogger` 不自己判断权限：控制台过滤器写入口由 `procfs` 检查
`cred().is_privileged()`，日志回读由 `ksyscall` 检查 `current_cred()`。
本模块只接受已经过这两道门的输入，因此它的安全目标是“被喂任何字符串
都不能破坏状态或内存”。

## 外部边界 / 攻击面

| 边界 | 来源 | 进入点 |
|------|------|--------|
| 启动元数据 | `/chosen/x-kernel,log-level`、命令行 `loglevel=`/`quiet`/`debug`、Kconfig `LOG_FILTER` | `set_filter_spec`（经 `kruntime`） |
| 用户输入（特权） | `echo '...' > /proc/sys/kernel/printk` | `set_filter_spec`（经 `procfs`） |
| 日志回读（特权） | `syslog(2)` / `dmesg` | `logbuf_len`、`logbuf_snapshot`、`logbuf_clear`，**用户拷贝在 `ksyscall` 内完成** |
| 日志正文 | 任意内核模块的格式化参数 | `Log::log` |
| 输出通道 | `kiface_logging_api::LogSinkIf` / `LogContextIf` provider | `print_fmt` |

不涉及用户内存指针、MMIO、DMA、FFI 或文件系统数据；`klogger` 内部没有
`unsafe` 代码块。

## unsafe 代码清单

无。全部状态操纵都是安全 Rust：定长数组、索引取模、`SpinNoIrq` 守卫。

## 内存安全不变量

1. `LineBuf.len <= LOGBUF_LINE`，`write_str` 只复制 `min(剩余空间, 输入长度)`
   字节；即使格式化过程被截断，`as_bytes()` 仍指向已初始化前缀。
2. `LogBuf.len <= LOGBUF_SIZE`，`start < LOGBUF_SIZE`；`push` 逐字节写入时
   用 `% LOGBUF_SIZE` 归一化索引，`snapshot` 用 `skip = len - take` 保证
   只复制有效尾部。
3. `FilterRule.target_len <= MAX_FILTER_TARGET`，由 `set_filter_spec` 校验；
   `level_for` 用 `&rule.target[..target_len]` 切片，`from_utf8` 失败仅跳过
   该条规则，不会 panic。
4. `FilterSpecWriter` 只在 `len + text.len() <= buf.len()` 时复制，越界返回
   `fmt::Error`，`filter_spec` 转成 `None` 交给调用者。

## 线程安全

- `FILTER`、`LOGBUF` 与 `print_fmt` 的锁都是 `SpinNoIrq`，可安全地在
  多核、中断上下文并发访问；`Filter`/`LogBuf` 均为 `Copy`/`Send` 的纯数据。
- 三把锁不嵌套持有：`Log::log` 先取 `LOGBUF` 短暂加锁写入并立即释放，
  再取 `FILTER` 判断，最后 `print_fmt` 取控制台锁。因此日志路径不会因为
  锁顺序产生环。
- `syslog(2)` 的读路径同样先取 `LOGBUF` 快照到内核缓冲，出锁后再访问用户
  内存，避免“持锁缺页”导致的长时间关中断或死锁。
- 无重入保护：`LogSinkIf::write_str` 内部若再打日志会递归取控制台锁；
  因此 provider 侧（`kruntime`）必须把控制台写做成不触发日志的路径。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 非法 filter spec 破坏日志可用性（把内核打成静默） | 中 | 设备树/命令行/特权用户写入畸形 spec | spec 先整体校验再换入；失败只告警并保留旧过滤器；空 spec 也拒绝 |
| T-02 | 覆盖规则绕过默认等级，意外打开热路径 `trace` | 低 | spec 中写出 `target=trace` | 天花板取所有规则最大值，只放宽该 target；`kipi` 等热路径日志已降为 `trace` |
| T-03 | 日志洪水覆盖启动证据 | 中 | 大量日志写入 64 KiB 环 | 环满即覆盖最旧字节；缓冲区读侧只暴露最新尾部；无速率限制（见已知限制） |
| T-04 | 未授权读取内核日志泄露地址/路径 | 中 | 非特权进程调用 `syslog(2)` | `ksyscall::sys_syslog` 先做 `is_privileged()` 检查，失败返回 `OperationNotPermitted` |
| T-05 | 非特权进程改动全局日志等级 | 中 | 写 `/proc/sys/kernel/printk` | `procfs` 写路径要求 `cred().is_privileged()`；节点权限为 owner 读写、其他只读 |
| T-06 | 超长日志记录丢失关键前缀或破坏行边界 | 低 | 模块打印超长消息 | `LineBuf` 截断到 256 B，记录仍带 `<level>` 前缀和换行，不破坏相邻记录 |
| T-07 | 快照从多字节 UTF-8 中间开始，`dmesg` 输出非法序列 | 低 | 取尾部时切断字符 | 缓冲区按字节存取，用户态工具自行处理；内核侧不解析正文 |
| T-08 | 日志正文伪造 `<n>` 等级前缀，欺骗 `dmesg` 过滤 | 低 | 模块打印以 `<4>` 开头的消息 | 每行前缀由 logger 统一生成；正文中的 `<n>` 只会出现在行内，`dmesg` 仅在行首解析 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 启动日志过滤器始终为默认值 | DT/命令行/Kconfig 都非法或缺失 | 日志按 `LOG_LEVEL_*` 输出 | 无 | 4 | 每级非法输入都 `warn!`，最终回退到 Kconfig 等级，不会静默 |
| F-02 | 缓冲区持续满、早期日志被覆盖 | 启动期日志量超过 64 KiB | 只保留最近记录 | 事件证据缺失 | 3 | 环容量与捕获等级解耦；需要完整启动日志时用串口/`LOG_FILTER` 定向捕获 |
| F-03 | `syslog(2)` 拷贝用户缓冲失败 | 传入不可写地址 | 返回 `EFAULT`，缓冲区不被清除 | 用户态 `dmesg` 失败重试 | 3 | 先取快照再拷贝；`READ_CLEAR` 只在拷贝成功后清除 |
| F-04 | 过滤器写锁与日志热路径互相等待 | 在 `FILTER` 临界区内再打日志 | 潜在关中断自旋 | 系统卡死 | 1 | 临界区内只做定长数据操作，`warn!` 等告警在锁外发出 |
| F-05 | 控制台 provider 未就绪 | `init_klogger` 前/`LogSinkIf` 未链接 | 记录只进缓冲区 | 串口无输出，`dmesg` 仍可读 | 4 | `log` 宏在无 logger 时为空操作；provider 在 `kruntime` 启动早期链接 |

## 故障管理

- 过滤器非法输入：`FilterSpecError`，调用者（`kruntime`、`procfs`）转成
  `warn!` 或 `VfsError::InvalidInput`，内核继续使用旧过滤器。
- `init_klogger` 安装全局 logger 失败会 `unwrap` panic：该路径没有错误返回
  或恢复手段，属于“主核启动不可恢复”的显式例外（见代码中的 `#[expect]` 理由）。
- 记录格式化失败被忽略（`let _ = writeln!(..)`）：日志是尽力而为，报错会递归。

## 隐私分析

日志正文可能包含内核地址、模块路径、任务名/ID 与设备状态；它们只在内核
控制台（物理可达的串口/显示）与特权进程的 `syslog(2)` 读路径出现。本模块
不采集用户隐私数据，也不把日志写入持久存储。

## 已知限制

- 没有日志速率限制：短时间内大量记录会用最新内容覆盖全部旧记录。
- 单条记录最长 256 B，超出部分截断，`dmesg` 看到的是被截断的行。
- `filter_spec` 的规范化文本上限为 `MAX_FILTER_SPEC`（256 B），规则多到
  接近上限时读侧返回 `None`。
- `SYSLOG_ACTION_CONSOLE_*` 未实现（等级域与 Linux 相反），控制台等级只
  通过 `/proc/sys/kernel/printk` 修改。
- 缓冲区固定 64 KiB、静态分配，不做大小配置。

## 审计清单

- [ ] 新增过滤器入口是否复用 `set_filter_spec`，而不是直接改 `FILTER`/`log::set_max_level`？
- [ ] 新的 spec 语法是否同时校验“空 spec”“超长 target”“规则条数超限”？
- [ ] 新的日志消费端是否在持锁期间访问用户内存或可能睡眠？
- [ ] 新增日志点是否可能被热路径高频触发（应使用 `trace!`）？
- [ ] 缓冲区读路径是否仍先做权限检查（`ksyscall`）？
- [ ] 行缓冲/环缓冲的索引算术是否仍然有界（`target_len`、`len`、`start`）？