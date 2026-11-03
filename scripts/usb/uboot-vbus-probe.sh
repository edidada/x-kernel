#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# Drives the USB VBUS enable line (gpio3.15, firmware's `vcc5v0-host`) from the
# **U-Boot prompt** and reports what the hardware does.
#
# Why: the kernel reads gpio3 fine but every write is dropped, so the ports never
# get 5V and no device can attach (`ccs` stays false however long a stick sits in
# the socket). U-Boot may run at a higher exception level than the kernel, so the
# same write from there separates "this bank rejects EL1 writes" from "the bank
# is fine and the kernel does something wrong".
#
# The commands are written with the sequence the board notes require: one command
# per prompt, because back-to-back lines get truncated and the tail runs as a
# command of its own.
#
# Usage:
#   scripts/usb/uboot-vbus-probe.sh          # power cycle, probe, power off
#   KEEP_ON=1 scripts/usb/uboot-vbus-probe.sh
#
# The board console is resolved by USB serial (A5XK3RJT = x-kernel board).

set -euo pipefail

RIG=${RIG:-kylin@172.30.193.53}
XK_SERIAL=${XK_SERIAL:-A5XK3RJT}

# Registers: SWPORT_DR, SWPORT_DDR, EXT_PORT of gpio3 (bank @ 0xfec40000).
read -r -d '' REMOTE <<'PY' || true
import sys, time
sys.path.insert(0, "/home/kylin/rkt")
from rkctl.console import Console
from rkctl.config import Config
from rkctl.power import make_power
import glob

def find_console():
    # The by-id symlink is stable across re-cabling (ttyUSB numbers follow
    # enumeration order); pyserial opens symlinks happily.
    for link in glob.glob("/dev/serial/by-id/*A5XK3RJT*"):
        return link
    return "/dev/ttyUSB1"

port = find_console()
print(f"== console {port}, power cycling")

cfg = Config(None)
power = make_power(cfg)
power.cycle()
time.sleep(0.2)

with Console(port, 1500000, "/tmp/usb-vbus-probe") as console:
    # This U-Boot does not print an obvious "Hit any key" banner before the
    # countdown, and a single key sent late loses the race to the kernel: hammer
    # the console from the first moment and stop as soon as the `=>` prompt is up.
    stopped = False
    for _ in range(60):
        console.send(" ")
        time.sleep(0.1)
        if "=>" in console.text():
            stopped = True
            break
    if not stopped:
        raise SystemExit("U-Boot prompt not reached (banner never appeared)")
    print("== stopped at U-Boot")

    for cmd in [
        "md.l 0xfec40000 1",   # DR
        "md.l 0xfec40008 1",   # DDR
        "md.l 0xfec40070 1",   # pad
        "mw.l 0xfec40008 0x00009800",  # DDR |= bit15 (keep the existing bits)
        "mw.l 0xfec40000 0x00008800",  # DR  |= bit15
        "md.l 0xfec40000 1",
        "md.l 0xfec40008 1",
        "md.l 0xfec40070 1",
    ]:
        console.send_line(cmd)
        time.sleep(0.6)
    time.sleep(1.0)
    text = console.text()
    print("== transcript tail")
    print("\n".join(text.splitlines()[-40:]))

if not (len(sys.argv) > 1 and sys.argv[1] == "keep"):
    power.off()
    print("== powered off")
PY

ssh -o BatchMode=yes "$RIG" "python3 - $( [[ ${KEEP_ON:-0} == 1 ]] && echo keep )" <<<"$REMOTE"
