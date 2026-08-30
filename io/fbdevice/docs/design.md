# fbdevice — 设计文档

## 定位

`fbdevice` 在主显示设备的 scanout 接口上提供 Linux fbdev 兼容层：分配 BGRA8888
shadow buffer，绑定为 host scanout resource，并把 `/dev/fb0` 的读写、mmap 和
ioctl 接到这块 shadow 上。

## 背景

virtio-gpu 没有直接映射的 framebuffer。Links 2.30 等程序通过 `mmap(/dev/fb0)`
改像素，且不一定调用 `write` 或 `FBIOPAN_DISPLAY`。shadow 必须在 mmap 打开期间
被周期性地 `present` 到 scanout，否则首屏和局部更新都不可见。

## 范围

- `src/lib.rs`：shadow 分配、scanout resource、present、mmap 刷新任务、DRM 抢占标志。

## 架构

```text
/dev/fb0  (devfs)
    |  read/write/mmap/fsync/FBIOPAN_DISPLAY
    v
fbdevice shadow (GlobalPage, BGRA8888)
    |  fb_present / fb-refresh task
    v
DisplayDevice::present_scanout_resource
    v
virtio-gpu SET_SCANOUT
```

DRM 路径通过 `drm_claim_scanout` / `drm_release_scanout` 声明占用。占用期间
fbdev 刷新和显式 present 都跳过，避免和 Weston 抢同一 scanout。

## 调用约束 / 执行上下文

- `fb_init` 在任务系统起来之后、显示设备已注册时调用。
- `fb_present` 可在任务上下文调用；刷新任务是普通 ktask，会 `sleep`。
- 不在中断上下文调用 `fb_present`。
- `/dev/fb0` 的 `open`/`release` 增减 client 计数；刷新任务在首次 open 时启动，
  之后常驻：有 client 且 DRM 未占用时每 33ms present，否则每 250ms 睡眠。

## 算法流程

1. 打开 `/dev/fb0`：client++，必要时 spawn `fb-refresh`。
2. mmap 写入直接落到 shadow（无 Rust `&mut [u8]` 独占假设，使用裸指针拷贝）。
3. `write` / `FBIOPAN_DISPLAY` / `fsync` 立即 `fb_present`。
4. 刷新任务在打开期间补上纯 mmap 路径的更新。
5. 关闭最后一个 fd：client--，任务转入 idle 睡眠。
6. DRM `SET_MASTER` 或成功 present 后 claim scanout；`DROP_MASTER` 或最后一个
   card0 fd 关闭后 release。

## 并发模型

- `FB` 用 `SpinNoIrq` 保护 Option\<Arc\>；present 先 clone Arc 再丢锁。
- client 计数和 DRM owner 用独立原子变量，避免在刷新循环里持锁睡眠。
- shadow 允许 mmap、read、write 并发访问，语义对齐 legacy fbdev 的数据竞争。

## 设计决策

- 不做完整 Linux deferred I/O / dirty-page 跟踪：实现简单，33ms 对图形浏览器足够。
- 空闲时 250ms 睡眠，避免无界 busy loop。
- 不在 `fb_init` 时启动刷新，避免无 client 时抢 DRM compositor。

## Drop / 资源释放

shadow `GlobalPage` 与 scanout resource 在仿真生命周期内常驻，没有 teardown。
刷新任务不退出，避免 “task 退出窗口内新 open 未再 spawn” 的竞态。
