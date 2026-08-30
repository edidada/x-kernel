# fbdevice — 安全与可靠性分析

## 信任模型

`/dev/fb0` 对用户空间暴露整块 shadow。内核不把 mmap 写入当作可信的结构化输入；
只保证访问不越出已分配的 shadow 范围。

## 外部边界 / 攻击面

- 用户 `read`/`write` 的 offset 和长度。
- `mmap` 把物理连续 shadow 映射进用户地址空间。
- framebuffer ioctl 的用户指针。
- DRM 与 fbdev 对同一物理 scanout 的竞争。

## unsafe 代码清单

| 位置 | 不变量 |
|------|--------|
| `copy_from_shadow` / `copy_to_shadow` | `src`/`dst` 指向 `fb_init` 分配且内核生命周期内有效的 shadow；offset+len 已夹紧在 buffer 内 |
| `/dev/fb0` 调用上述拷贝 | `FrameBuffer::{base,size}` 来自同一 shadow |

不构造覆盖整块 framebuffer 的 `&mut [u8]`，避免与 mmap 共享内存冲突。

## 内存安全不变量

- shadow 由 `GlobalPage` 持有，直到内核关机。
- 拷贝路径拒绝 `offset >= size` 的 write（`ENOSPC`），read 则返回 0。

## 线程安全

mmap、read、write、刷新任务可以同时碰同一字节。这是 fbdev 的既有模型，不是
Rust 别名安全；显示内容可能撕裂，但不会破坏内核对象。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 越界读写 shadow | 高 | 错误的 offset/len | 边界检查后再 `copy_nonoverlapping` |
| T-02 | 刷新任务 busy loop | 中 | 无 sleep 的 present 循环 | 有 client 时 33ms，否则 250ms |
| T-03 | 与 DRM compositor 抢 scanout | 中 | Weston 与 Links 同时运行 | DRM claim 后跳过 fb present |
| T-04 | 伪造 ioctl 用户指针 | 高 | 无效地址 | `write_vm`/`read_vm` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | present 失败 | host 忙 | 一帧丢失 | 画面延迟 | 4 | 返回 false，下一轮再试 |
| F-02 | 刷新任务未启动 | 首次 open 未计数 | mmap 不可见 | Links 黑屏 | 2 | open 时 `fetch_add` 后 `start_refresh_task` |
| F-03 | DRM 退出后 fbdev 失效 | 未 release owner | 第二次 Links 黑屏 | 显示死掉 | 2 | DROP_MASTER 与最后一个 card0 close 都 release |

## 已知限制

- 无脏页跟踪，整屏 present。
- 单 scanout；不能同时正确显示 DRM compositor 和 fbdev。
- mmap 关闭 fd 后仍保持映射时，client 计数可能已归零，刷新停止。Links2 保持 fd 打开。

## 审计清单

- 每次 shadow 拷贝是否经过 offset 边界检查。
- 是否在持锁状态下 `sleep` 或 `present`。
- DRM owner 的 claim/release 是否覆盖 SET_MASTER、present 和 last close。
