# Links 2.30 framebuffer Demo

在 x-kernel QEMU 图形环境下运行未改源码的 Links 2.30 framebuffer 模式，
用 HTTPS 打开网页，并用鼠标、键盘做基本交互。中文依赖 guest 里的文泉驿字体。

## 最终运行命令

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config   # 若尚无 .config
make defconfig
make build
make uapps UAPPS=hello,fb-mmap-refresh,mousedev-ps2,kbd-probe,links2
make run GRAPHIC=y
```

`make uapps` 写 `disk.img` 时不要同时开 QEMU。装完后用 `debugfs dump` 和主机文件做字节比对，
不要只看 inode Size（inode 复用时 debugfs `write` 可能只写出第一块）。

Guest（autostart 会跑 `links-demo`；也可手动）：

```sh
mousedev-ps2
fb-mmap-refresh
xklinks -dump https://example.com
xklinks -dump file:///usr/share/links-demo/cjk.html
xklinks -g file:///usr/share/links-demo/cjk.html
```

图形界面里：

- `g` 打开 URL 对话框，输入完整地址，例如 `https://www.baidu.com`（不要只写主机名）
- Tab / 回车跟链接，`z` 后退
- 不要按 `\`，那会切到源码视图（满屏 CSS）
- 看 Links 画的白箭头，不是宿主机光标
- 拖 QEMU 窗口可以放大画面；**Ctrl+Alt+G** 放开抓取后再拖边框

`xklinks` 是静态包装器，会设 fontconfig、把各字重都指到文泉驿 TTC，并打开内置 CA。
直接跑 `/usr/local/bin/links` 也能浏览英文，但标题等粗体会变成 `*`。

可选稳定性 / 键盘探针（不要和正在运行的 Links 抢同一 TTY）：

```sh
kbd-probe          # 配合主机 QMP 注入按键；单独跑会 8s 超时
links-fb-verify    # 两次启动 Links 并采样 /dev/fb0
links-e2e          # 上面两项
```

无窗口抓串口时：

```bash
timeout 90 make justrun GRAPHIC=y QEMU_ARGS="-display none"
```

带 QMP 截屏和键鼠注入（主机）：

```bash
timeout 180 make justrun GRAPHIC=y \
  QEMU_ARGS="-display none -qmp unix:/tmp/xk-links.qmp,server,nowait"
# 另开终端：
python3 uapps/links2/host-qmp-e2e.py \
  --qmp /tmp/xk-links.qmp \
  --log target/xkmake/kplat-aarch64/release/qemu.log \
  --out-dir /tmp/xk-links-demo
# 若 guest 先跑 kbd-probe，再加 --kbd-probe
```

`GRAPHIC=y` 会加上 `virtio-gpu`（默认 1920x1080）、`virtio-keyboard`、`virtio-mouse`，
以及 `-display gtk,zoom-to-fit=on`。拖动 QEMU 窗口会缩放画面；菜单 **View → Zoom In / Zoom Out**
也可以。全屏：先 **Ctrl+Alt+G** 放开鼠标抓取，再 **Ctrl+Alt+F**。
WSL/WSLg 不会按 Windows 的 DPI 自动放大 guest 像素，所以以前 1280x800 会显得很小。
需要覆盖显示时用 `QEMU_ARGS='-display none'` 等。

Weston autostart 已关闭，避免抢走 scanout。需要时手动
`/usr/local/bin/xk-weston-start`。

成功串口摘录见 `docs/links2-framebuffer-demo-run.log`。

## 修改模块

- `io/ktty`：单控制台 `VT_*` / `KD*` ioctl；console TTY 从 virtio-keyboard 取字节
- `io/fbdevice` + `fs/filesystems/devfs` `/dev/fb0`：按 offset 的 read/write、
  无 `&mut [u8]` 别名、mmap 周期刷新、`fsync` present
- `io/drmdevice`：`SET_MASTER` / last close 与 fbdev 协调 scanout
- `fs/filesystems/devfs` `/dev/input/mice`：PS/2 mousedev（不再复用 evdev）
- `io/inputdev`：设备 snapshot、键盘 EV_KEY → TTY 字节、16ms `kbd-wake`
- `xtask/xkmake`：`GRAPHIC=y` 附加 virtio-keyboard / virtio-mouse
- `entry`：无 `/dev/tty1` 时不要把 PID 1 交给会饿死的 OpenRC getty
- `uapps/`：`links2`（OpenSSL、FreeType、Fontconfig、文泉驿、`xklinks`）、
  `fb-mmap-refresh`、`mousedev-ps2`、`kbd-probe`；Weston autostart 关闭

## Links2 需要的 Linux compatibility 语义

- `VT_GETMODE` / `VT_SETMODE`（`VT_PROCESS`）
- `VT_WAITACTIVE(1)`、`VT_GETSTATE`、`VT_RELDISP`（`VT_ACKACQ`）
- `KDSETMODE` / `KDGETMODE`
- `/dev/fb0`：`FBIOGET_VSCREENINFO`、`FBIOGET_FSCREENINFO`、`FBIOPAN_DISPLAY`、
  线性 mmap、`fsync` 刷新
- `/dev/input/mice`：一次 write 覆盖式 ACK、IMPS/EXPS 探测、3/4 字节 PS/2 包、短读
- raw TTY 键盘（stdin fd 0），不是 evdev
- 编译期需要 `gpm.h` + `-lgpm`；运行期 `Gpm_Open` 失败即可走 mice

## 实际新增的 syscall / ioctl / device ABI

无新 syscall。新增 ioctl：`VT_GETMODE`、`VT_SETMODE`、`VT_GETSTATE`、
`VT_WAITACTIVE`、`VT_ACTIVATE`、`VT_RELDISP`、`KDSETMODE`、`KDGETMODE`。
`/dev/input/mice` 改为 mousedev 字符设备（major 13, minor 63）。

## framebuffer 刷新

打开 `/dev/fb0` 期间，`fb-refresh` 每 33ms `present` shadow（DRM 占用时跳过）。
`write`、`FBIOPAN_DISPLAY`、`fsync` 立即 present。空闲 250ms 睡眠。

## mousedev 数据路径

```text
virtio-mouse
  → input class device
  → mousedev (PS/2 IMPS/EXPS)
  → /dev/input/mice
  → Links2
```

键盘：

```text
virtio-keyboard
  → input class device
  → TTY 字节映射（跳过带 BTN_MOUSE 的设备）
  → /dev/console stdin
  → Links2 handle_svgalib_keyboard
```

## 已通过测试

实测平台：WSL2，`qemu-system-aarch64` TCG，`GRAPHIC=y`，1280x800。

| 项 | 证据 |
|----|------|
| Stage 1 VT | 无 `Could not get VT mode`；出现 `CSI 10000B/C` 藏光标并打开 `/dev/fb0` |
| Stage 2 mmap 刷新 | `fb-mmap-refresh: mmap write visible in offset readback (1280x800)` |
| Stage 3 鼠标 | `mousedev-ps2: IMPS id=3 EXPS id=4 short-read ok`；QMP 移动后 scanout 差 2214 字节 |
| Stage 4 键盘 | `kbd-probe: got 2 bytes: 67 6f`（QMP 注入 `g`/`o`）；随后 `g/tab/ret/esc` 再差 4179 字节 |
| Stage 5 HTTPS | `xklinks -dump https://example.com` 打出 Example Domain |
| Stage 6 CJK dump | `xklinks -dump file:///usr/share/links-demo/cjk.html` 打出「你好，世界」 |
| Stage 7 CJK 图形 | QMP 截屏可见大标题「你好，世界」和正文汉字，不是 `*`；light≈1.02e6 |
| Stage 8 重启 | 此前 `links-fb-verify` first/second 均 painted，无 panic |
| QEMU 键鼠设备 | 命令行含 `virtio-gpu-pci`、`virtio-keyboard-pci`、`virtio-mouse-pci` |
| QMP screendump | 1280x800，magenta=0 |

`xtask/xkmake` 单测 `graphic_mode_adds_keyboard_and_mouse` 覆盖 GRAPHIC 设备列表。

Links 交叉编译：`HAVE_SSL`、`HAVE_OPENSSL`、`HAVE_FREETYPE`、`GRDRV_FB`；
OpenSSL 3.0.15 静态库，`OPENSSLDIR=/etc/ssl`。

## 仍存在但不影响 Demo 的问题

- 不是完整 Linux VT / mousedev；无多控制台切换，不发送 relsig/acqsig
- 无 dirty rectangle，整屏 present
- 键盘是最小 US 布局，不是完整 keymap
- busybox 部分 applet（`env`、`wget`）在当前 rootfs 上会 `I/O error`；探针用静态 `xklinks` / `links`
- Alpine 无 `/dev/tty1` 时不能走 OpenRC getty；`init.sh` 回退到 `/dev/console` login shell
- QMP `rel` 轴没有 `wheel`，滚轮用 `btn` 的 `wheel-down` / `wheel-up`
- 无界 busy loop 已避免；空闲刷新 250ms
- Links 本身几乎不跑 JS、CSS 也很弱；百度首页能打开，但会跟一堆 CSS/图片，看起来乱
- 相对鼠标 + 33ms 刷新 + TCG，光标会有偏差和延迟
- Links 2.30 的 `-font-monospaced "path @ index"` 会把带 `@` 的原串传给 `FT_New_Face`，打不开；包装器不用这种写法
- 只写 `www.baidu.com` 时 Links 走 http://。图形模式还会并行拉图片/CSS，个别连接对端关掉就会在页面上显示 `Broken pipe`；请用 `https://www.baidu.com`。`xklinks` 已固定 IPv4，并关掉图形图片加载。
