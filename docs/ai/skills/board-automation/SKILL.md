# Board Automation And Reference Dumps (rk3588)

Use this skill whenever a change must be validated on the real RK3588 board
instead of QEMU, and whenever a driver "hangs on hardware" needs a
register-level answer. It exists because the same three mistakes cost hours
each time: forgetting to use the automation at all, running the unittest
suite on a board config, and guessing at hardware state instead of dumping a
working reference next to the broken one.

## 1. The loop: never boot the board by hand

`scripts/board-run.sh <tag>` does the whole cycle:

```
build -> deploy to the TFTP root -> power off -> power on -> wait for boot
      -> capture the console -> fetch <tag> logs back -> report
```

- `SKIP_BUILD=1 scripts/board-run.sh <tag>` reuses the last image (the common
  case while bisecting one line).
- A run that ends in `ERR 240001ms 等启动超时` is the *normal* signature of a
  kernel that hung: the boot never settles, so the harness times out. Read the
  fetched `console.log`, do not re-run blindly.
- Everything under `~/rkt/` on the test host: `./rkt run --tag <tag>
  --until-quiet 30 --no-preflash` for a one-off run, `./rkt tftp start` to
  bring the TFTP server back, `./rkt console --seconds N` to just listen,
  `./rkt power on|off` for the relay.

### 0. Install the stable names first (one-time, per rig)

`/dev/ttyUSB<N>` is assigned in enumeration order, so **manually re-plugging
one cable renumbers all three ports**. That is not hypothetical: one board
run captured the native-Linux reference board and produced an empty log
because the console had moved from `ttyUSB0` to `ttyUSB2`.

The repo pins the three adapters to stable names via udev:

```
scripts/board-udev/99-xkernel-rk3588.rules   # the rule (versioned)
scripts/board-udev/install.sh                # install on the rig over ssh
scripts/board-udev/install.sh --local        # or on this host
scripts/board-udev/install.sh --patch-rkt    # then aim rkt at the names
```

| stable name | role | match key |
|---|---|---|
| `/dev/ttyXK` | **our (x-kernel) board's console** | FTDI `0403:6001`, serial `A5XK3RJT` |
| `/dev/ttyREF` | the native-Linux reference board | FTDI `0403:6001`, serial `A54LQ9PF` |
| `/dev/ttyPWR` | relay / USB power switch | CH340 `1a86:7523` |

Installation needs root on the rig host, so `install.sh` prompts for its
sudo password — **do not try to guess or automate it**; that account locks
after a few wrong attempts. `--patch-rkt` needs no root (it only edits
`~/rkt/profiles/opi5plus.ini`).

Match keys: the two FTDI adapters share vendor:product, so only the factory
serial distinguishes them; the serials are printed on the FTDI shells and
`ls -l /dev/serial/by-id/` shows them. Re-plugging an adapter elsewhere
keeps working; **swapping in a different adapter requires updating the
serial in the rule**.

After the rule is installed, point rkt at the stable names by editing
`~/rkt/profiles/opi5plus.ini` (`console.port = /dev/ttyXK`,
`power.port = /dev/ttyPWR`) — not via `rkt --console`, see below.
`scripts/board-run.sh` prints an adapter check before each run so a
renumbered port is visible before the cycle starts.

### Serial topology (get this wrong and you debug the wrong board)

The rig drives **two** boards plus a relay, and the three ports are NOT
interchangeable:

| raw node (today) | stable name | role | baud |
|---|---|---|---|
| `/dev/ttyUSB0` | `/dev/ttyXK` | **our (x-kernel) board's console** — the one `board-run.sh` captures | 1_500_000 |
| `/dev/ttyUSB1` | `/dev/ttyPWR` | relay / USB power control | 115200, frame `AA 01 03 AE` on, `AA 01 02 AD` off |
| `/dev/ttyUSB2` | `/dev/ttyREF` | the **reference native-Linux board** (comparison only) | 1_500_000 |

The raw node numbers are what they happened to be on the last enumeration;
treat them as volatile and use the stable names.

Consequences worth remembering:

- Capturing the reference board while trying to test a kernel change gives
  an empty or irrelevant log — it is a *different machine*. One run was
  wasted this way; that is why the stable names above exist.
- The ports can renumber when things are re-cabled; the relay has been seen
  on `/dev/ttyUSB3`. Enumerate before concluding "the board is dead":

  ```bash
  ssh kylin@<host> 'python3 - <<EOF'
  import serial, glob, time
  for p in sorted(glob.glob("/dev/ttyUSB*")):
      for b in (1500000, 115200):
          s = serial.Serial(p, b, timeout=0.8); s.write(b"\n"); time.sleep(0.7)
          print(p, b, repr(s.read(4096)[-80:])); s.close()
  EOF'
  ```

- `console_open ERR ... Could not exclusively lock port /dev/ttyUSB0` is a
  **transient host-side lock**, not a missing board: a desktop agent (seen:
  Sangfor `eaio_agent`) grabs the port for a moment. Simply retry the run.
  Do not "fix" it by switching ports.
- `rkt --console ...` is a *global* option: it must precede the subcommand
  (`rkt --console /dev/ttyUSB2 run ...`), and at least one rkt build then
  crashed with `'Console' object has no attribute '_start_wall'` — that one is
  a rig bug with a one-line fix, see §5.4. Prefer driving the loop through
  `scripts/board-run.sh` unless you really need the override.
- The single-board alternative when rkt misbehaves: power-cycle with
  `rkt power off/on`, then capture with pyserial on `/dev/ttyUSB0` directly.
- **The TFTP server dies with the SSH session** that started it. If a run
  fails with `UDP 69 没有监听`, restart `rkt tftp start` before anything else.
- Every `board-run.sh` finishes by powering the board off. Power it back on
  (`rkt power on`) before trying to talk to a stock Linux on the same board.

## 2. The unittest trap (repeat of the project rule)

`make UNITTEST=y unittest` **must** run on a QEMU config. With the rk3588
config in `.config` the suite dies silently and looks like a code failure:

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config && make defconfig
make UNITTEST=y unittest
```

The Makefile carries a `config-guard-qemu` pre-step that now fails loudly
(`.config is not the QEMU machine config`) instead of hanging. When you see
that message, it is the config, not the code.

## 3. Comparing against a working kernel (the decisive technique)

A hang with no panic on real silicon is usually "the peripheral has no
clock / is held in reset". Guessing costs a 6-minute board cycle per guess;
a register dump of a *working* kernel on the *same* board settles it in one
shot.

1. Boot the board into a stock Linux (BSP image) and give it root. On the
   Orange Pi 5 Plus image the `orangepi` account's sudo password is
   `orangepi`:

   ```bash
   echo orangepi | sudo -S -p "" true; echo RC=$?   # RC=0 => usable
   ```

2. Read MMIO with `/dev/mem` (busybox `devmem` is usually absent):

   ```python
   import mmap, os, struct
   f = os.open("/dev/mem", os.O_RDONLY | os.O_SYNC)
   def rd(pa):
       m = mmap.mmap(f, 0x1000, mmap.MAP_SHARED, mmap.PROT_READ, offset=pa & ~0xfff)
       return struct.unpack("<I", m[pa & 0xfff:][:4])[0]
   ```

3. `dmesg` is readable and is the cheapest source of truth about a working
   driver: `dmesg | grep -i pcie` shows the link state and each controller's
   LTSSM at probe time. `/sys/kernel/debug/clk/clk_summary` (root) gives the
   **actual expected rates** of every clock a driver enables.

4. Run the same reads *inside our kernel* (an `iomap_region` plus `read32`
   in the driver), log both sides at the same offsets, and diff.

### Reference values recorded from stock Linux 6.1.99 on Orange Pi 5 Plus

PCIe controller `fe190000` (pcie2x1l2, RTL8125 behind it, link up):

```
DBI   0xa41000000 = 0x35881d87          (Rockchip 0x1d87:0x3588)
APB   0x00 = 0x0000004c   0x04 = 0x00000004   0x10 = 0x00000003
      0x14 = 0x00000122   0x18 = 0x00007737   0x20 = 0x0000177f
      0x24 = 0x0000fffb   0x28 = 0x000001ff   0x2c = 0x00002008
      0x30 = 0x04401812   0x180 = 0x00000012  0x300 = 0x00130014
CRU   0x440 = 0x00000288 (aclk_php_root: gpll, /3 = 396 MHz)
      0x880 = 0x0000ec18   0x884 = 0x0000739c   0x888 = 0x00000d0e
      0x898 = 0x000043d0   0xa84 = 0x00005000   0xa88 = 0x00000000
PMU   0x10c = 0x000017ff   0x118 = 0x00081fff   0x120 = 0x00081fff
      0x150 = 0x000004c1   0x180 = 0x04c1ffff   0x290 = 0xfe9f0001
combphy0 (0xfee00000)  0x04 = 0x80  0x08 = 0xff  0x18 = 0x88  0x20 = 0x60
      0x28 = 0x90   0x2c = 0x43   0x30 = 0x88   0x34 = 0x56
pipe-phy-grf (0xfd5bc000)  0x00 = 0x1000  0x04 = 0x4000  0x08 = 0x101
      0x0c = 0x200  0x34 = 0x80
clock rates: aclk_pcie_1l2_dbi/slv = 396 MHz, aclk_pcie_1l2_mstr = 500 MHz,
      pclk_pcie_1l2 = 150 MHz, clk_pcie_aux4 = 24 MHz,
      pclk_pcie_combo_pipe_phy0 = 100 MHz
```

The lesson these numbers encode: `APB 0x300` (LTSSM) reading **0** while the
board's device is present means the link is not trained at all, and the
`0x04/0x10/0x14/0x30` words (the "current limit" set the vendor driver logs)
stay 0 until the PHY actually produces its pipe clock. A DBI read that stalls
the bus (no panic, no external abort) is the DWC slave running on that dead
pipe-derived core clock.

## 4. Reading the failure mode correctly

- **Synchronous external abort** (`ESR=0x96000010`, DFSC `0x10`) on an MMIO
  read means the bus returned a decode error: nothing is decoding that
  address (unpowered/unclocked block, wrong address).
- **Silent stall, no panic** means the transaction was accepted but never
  completed: the target exists and is clocked just enough to be reached, but
  its core is not running.
- Both are diagnostic gold. Bisect by reading the same address at different
  points of probe: *before* any clock/reset/PHY work an unowned controller
  aborts, *after* it stalls — that is what proves the remaining gap is the
  clock the core actually needs.
## 5. When the console is silent: five checks in this order

Everything in this section has the same symptom — **zero console bytes** — and
that is exactly what a hung or bricked kernel looks like. Run the checks in
order; only the last one is about the image. (Items 2–4 also appear in §1 as
one-line warnings; here they have the commands and the root cause.)

### 5.1 Which port, by USB serial — not by ttyUSB number

Numbering follows enumeration order and re-shuffles when a cable is re-plugged
or the hub re-enumerates. Measured once: rkt's `console` had become the
**reference board** and its `power` port had become our board's console, so the
run "powered on" fine (relay frames going into a UART) and then read nothing
for 240 s. `console.raw.log` was 0 bytes — the tell that no reader was at fault.

```bash
for d in /dev/ttyUSB*; do printf '%s ' "$d"; \
  udevadm info -q property -n "$d" | grep ^ID_SERIAL_SHORT=; echo; done
# A5XK3RJT      = our board's console   (FTDI 0403:6001)
# A54LQ9PF      = the reference board   (same model, same VID:PID)
# (no serial)   = CH340 relay           (1a86:7523)
```

### 5.2 Does the relay actually answer?

Being "on the right port" is not enough — confirm the device is a relay:

```bash
python3 - <<'PY'
import serial
p = serial.Serial("/dev/ttyUSB2", 115200, timeout=0.6)
p.write(bytes.fromhex("AA0105B0")); print(p.read(16).hex())   # -> aa0100ab
PY
```

A UART never answers this frame, so a non-reply proves the port is wrong
whatever `/dev` name it has.

`scripts/board-udev/install.sh` pins `/dev/ttyXK` (our console), `/dev/ttyPWR`
(relay) and `/dev/ttyREF` (reference board); `--patch-rkt` rewrites the profile
to use them. If those names are absent the rig has reverted (udev rules live in
`/etc/udev/rules.d`, not in the repo) — re-install rather than re-discovering
numbering.

### 5.3 Is TFTP listening? (it dies with the SSH session that started it)

`board-run.sh` assumes a server is already up. Restart it detached:

```bash
ssh kylin@rig 'bash -lc "cd ~/rkt && (setsid ./rkt tftp start > ~/tftp.out 2>&1 &); \
  sleep 4; ss -lun | grep \":69 \""'
```

### 5.4 The `_start_wall` crash is a rig bug with a one-line fix

```
boot  ERR  AttributeError: 'Console' object has no attribute '_start_wall'
```

`rkctl/console.py` sets `_t0` in `Console.__init__` but the silence check in
`wait_for` reads `_start_wall`, so **any** run with `quiet_warn_s` /
`--until-quiet` dies while reporting silence — after power-on, before the
verdict, leaving a one-line `console.log`. Fix it in place (back up first):

```python
self._t0 = time.time()
self._start_wall = self._t0   # silence window start (wait_for uses it)
```

Keep `--until-quiet`: it is what bounds a bus-stall hang. Dropping it to dodge
the bug trades a 45 s failure for a 240 s timeout.

### 5.5 Only then suspect the image

If ports are right, the relay answers, UDP 69 listens, and `console.raw.log`
still has no bytes, the board really is not talking. Fall back to §3 (compare
against a working kernel) or §4 (classify the failure mode).

### 5.6 The relay can be pinned by USB id, the console cannot

`/dev/ttyUSB*` order changes on every power cycle because the CH340 sits behind
a hub that re-enumerates with the board. The relay is the only adapter with a
**unique** VID:PID (`1a86:7523`), so point the profile at a name that does not
exist and let rkt's id fallback find it:

```ini
[power]
port = /dev/ttyPWR        # never exists; forces the usb_id scan
usb_id = 1a86:7523
```

Both FTDI adapters are `0403:6001`, so the console **cannot** be pinned that
way -- its `port` must be re-checked by `ID_SERIAL_SHORT` each session (5.1).

### 5.7 Booting the board's stock Linux for a register diff

The SD's `/boot.scr` is the *tftp* script (it boots our kernel), so the normal
bootflow cannot reach the BSP Linux. Boot it by hand from the U-Boot prompt --
the manual `load`/`booti` path is the only reliable way in:

```
setenv bootargs console=ttyS2,1500000 root=/dev/mmcblk1p2 rw rootwait
load mmc 1:1 0x02080000 /Image
load mmc 1:1 0x0a000000 /dtb-6.1.99-rockchip-rk3588/rockchip/rk3588-orangepi-5-plus.dtb
booti 0x02080000 - 0x0a000000
```

Notes that cost time to rediscover:

- the U-Boot environment loads "from nowhere" (empty), so `boot-linux.scr`
  fails on unset `${scriptaddr}`/`${fdt_addr_r}`; drive `load` manually.
- send **one** command per `=>` prompt. Long lines sent back-to-back get
  truncated and the remainder is executed as a command (`booti` arrived as
  `load`, which prints the load usage text).
- the DTBs live under
  `/dtb-6.1.99-rockchip-rk3588/rockchip/` (not `/dtb/rockchip/`).
- it is an Ubuntu Jammy image (`Welcome to Orange Pi 1.2.2 Jammy`); the
  `orangepi` account's password is `orangepi`.

### 5.8 /dev/mem cannot read DRAM on the reference board

Reading a BAR through `sysfs resourceN` works; reading **System RAM** through
`/dev/mem` returns `PermissionError` on the vendor kernel (`STRICT_DEVMEM` /
the distro's lockdown). So "dump the working driver's descriptor ring from
DRAM" is not possible that way -- it needs a small loadable module on that
board. Reading a *device* window through /dev/mem is what produced the
SError panic earlier (§5.1 note): always prefer `resourceN`, and skip devices
whose BAR is unassigned instead of reading physical 0.
