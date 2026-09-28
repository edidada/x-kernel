# Thread 进程资源访问路径验证

日期：2026-09-28。

## 实现与来源

- 内核分支：`refactor/thread-resource-access`，从本地 `main` 的
  `4b38d72ec50c7ad665d52cf47a18c46199f986f3` 创建。
- 实现提交：`10faebbf7e1b179a1dc89adee2886763a72d2508`。
- harness 分支：`test/thread-resource-access`，测试提交：
  `df1de30b2057c398a928ed124a26a8b6a1f11541`。
- Linux 语义参考：`/root/codes/linux`，提交 `818bebeb63dd`，
  `fs/file.c::exit_files` 与 `__fget_light`。

同步文件资源访问现在沿 `Thread → 已持有的 ProcessRuntime → ProcessResources`
借用。`Thread::resources()` 返回与 `&self` 生命周期绑定的 `&Arc<ProcessResources>`；
现有 `current_resources()` 为需要独立所有权的调用者显式克隆该 Arc。
没有新增结构体、字段、缓存或 unsafe。多次查表的 poll、epoll、sendfile 等复用线程借用。

相较原路径，单次查找去掉 Process runtime_ref 锁、Weak 升级，
以及临时 Process、ProcessRuntime、ProcessResources 的强引用计数操作。
仍保留当前线程句柄、FD 表所有者锁、FD 表锁以及查到的文件 Arc。
跨进程资源访问仍使用 Process 的有错误返回的查找接口。

## 生命周期与回归

- runtime 存活只保证资源对象可借用，不能保证 FD 表仍附着。
- `exit_files`、FD 表替换、unshare 与并发关闭的同步实现未修改。
- 资源生命周期单测改从 Thread 借用，验证表替换后旧表回收，以及释放后访问失败。
- `make build`、`make clippy`、格式检查通过。
- `kprocess,kresources`：70 项单测通过，覆盖率文件成功生成。
- harness `ci-test-iter / fcntl-semantics`：3 项 guest 测试全部通过。
- 同样的 3 项测试在宿主 Linux 通过。
- 新增测试覆盖 `CLONE_FILES` 子进程关闭和退出不破坏父进程剩余 FD，
  以及 128 轮并发 pread/close（允许合法的读成功或 EBADF）。
- 修正旧 harness 的 F_GETLK 测试：无冲突时 Linux 保持输入 l_pid，
  原先要求清零的断言在宿主和 guest 都失败。参考 `fs/locks.c::fcntl_getlk`。
  此修正不改变内核行为。

主要命令：

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
cp --reflink=auto /root/codes/starry-test-harness/.cache/rootfs/rootfs-aarch64-xkernel-musl.img /tmp/thread-resources-unit.img
make unittest UNITTEST_CRATE=kprocess,kresources DISK_IMG=/tmp/thread-resources-unit.img

# harness 仓库
cargo test --manifest-path tests/cases/Cargo.toml --test fcntl_semantics -- --test-threads=1
CASES=fcntl-semantics ARCH=aarch64 ROOTFS_VARIANT=xkernel-musl \
  XKERNEL_REMOTE=/root/codes/2/x-kernel make ci-test-iter run
```

Guest 日志：
`/root/codes/starry-test-harness/logs/ci-test-iter/20260928-084310/cases/fcntl-semantics.log`。

## fio 对照方法

相同 AArch64 QEMU KVM、4 vCPU、1 GiB、musl rootfs、defconfig。
两次生成的配置逐字节一致，rootfs SHA-256 均为
`f20f7b41fa52aa38bda79e918e7c10651f35f6495e7826864cc863c7424e17ca`。
使用 harness 原有 `fio-ext4-baseline` / `fio-daily-v2`，完整执行 10 个场景。
4K 随机读写参数：psync、iodepth=1、direct=0、32 MiB 预写文件、
numjobs=1、randseed=20260908、30 秒测量 + 5 秒 ramp；读场景 pre_read=1。
这是缓存/缓冲 I/O 对照，不代表设备直接 I/O 性能。

```sh
CASES=fio-ext4-baseline ARCH=aarch64 ROOTFS_VARIANT=xkernel-musl \
  XKERNEL_REMOTE=/tmp/xkernel-thread-resources-main make daily-test run
CASES=fio-ext4-baseline ARCH=aarch64 ROOTFS_VARIANT=xkernel-musl \
  XKERNEL_REMOTE=/root/codes/2/x-kernel XKERNEL_REF=refactor/thread-resource-access \
  make daily-test run
```

基线日志：`/root/codes/starry-test-harness/logs/daily/20260928-083305/`。
修改版日志：`/root/codes/starry-test-harness/logs/daily/20260928-084512/`。
`20260928-084414` 轮次因提交钩子与构建元数据采集交错而主动终止，排除比较。

本次仅验证 AArch64，未运行其他架构。性能对照每版一个样本，未固定宿主 CPU
或频率；基线执行期间曾有本地编译，不能将单轮差值视为稳定加速比例。
结构上的锁和引用计数减少由调用路径确认，端到端收益以以下观测为限。

## fio 结果

两版完整 10 场景均通过，workload contract（fio/预写/任务文件哈希）一致。

| 指标 | main | 修改版 | 差值 |
|---|---:|---:|---:|
| rand-read-4k/read IOPS | 231209.460 | 263307.556 | +13.88% |
| rand-read-4k/read 平均延迟 µs | 3.891 | 3.378 | -13.16% |
| rand-read-4k/read P99 延迟 µs | 4.960 | 4.640 | -6.45% |
| rand-write-4k/write IOPS | 190481.716 | 215395.390 | +13.08% |
| rand-write-4k/write 平均延迟 µs | 4.709 | 4.157 | -11.72% |
| rand-write-4k/write P99 延迟 µs | 5.856 | 5.536 | -5.46% |

观察：本轮随机读写吞吐上升，且对应平均与 P99 延迟下降。
推断：结果与减少查表前锁及引用计数的预期方向一致，但单轮端到端测试
无法精确归因或证明长期稳定收益。没有修改 FD 表自身的同步策略。

原始 fio 数据、配置、构建信息、单元测试、guest 回归日志与两个仓库的补丁
收录于随附 `thread-resources-validation.tar.gz`。内核实现与 harness 测试
均已本地提交，未推送远端。
