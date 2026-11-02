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

### Serial topology (get this wrong and you debug the wrong board)

The rig drives **two** boards plus a relay, and the three ports are NOT
interchangeable:

| port | role | baud |
|---|---|---|
| `/dev/ttyUSB0` | **our (x-kernel) board's console** — the one `board-run.sh` captures | 1_500_000 |
| `/dev/ttyUSB1` | relay / USB power control | 115200, frame `AA 01 03 AE` on, `AA 01 02 AD` off |
| `/dev/ttyUSB2` | the **reference native-Linux board** (comparison target only) | 1_500_000 |

Consequences worth remembering:

- Capturing `/dev/ttyUSB2` while trying to test a kernel change gives an
  empty or irrelevant log — it is a *different machine*. One run was wasted
  this way.
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
  crashed with `'Console' object has no attribute '_start_wall'`. Prefer
  driving the loop through `scripts/board-run.sh` unless you really need the
  override.
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