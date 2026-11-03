#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# Reads the USB bring-up registers on the **native-Linux reference board** and
# prints a labelled table. Step 0 of the RK3588 USB port: our controller does
# not answer on the bus (synchronous external abort on the first read), and two
# independent questions can only be settled against a board where USB works:
#
#   1. PD_USB polarity -- pm-domains.c powers the domain up by writing only the
#      write-mask half of the register (`BIT(11) << 16`, low half zero), which is
#      the opposite of what our driver writes.
#   2. Whether the `usb3-otg` reset line we deassert is the one that gates the
#      register window, and whether the USB3 ports' VBUS is switched on at all
#      (`vcc5v0-host` is a GPIO-driven regulator we have no framework for).
#
# Usage:
#   scripts/usb/ref-usb-dump.sh              # drive the reference board console
#   scripts/usb/ref-usb-dump.sh --payload    # print the remote payload only
#   REF_CONSOLE=/dev/ttyUSB0 scripts/usb/ref-usb-dump.sh
#
# The board runs stock Ubuntu with auto-login as `orangepi`, and /dev/mem needs
# root: the payload goes through sudo (`REF_SUDO_PW`, default `orangepi`).
# `devmem` is not installed; reads go through python3 + mmap, which is also the
# only path that works with CONFIG_STRICT_DEVMEM for device memory.

set -euo pipefail

RIG=${RIG:-kylin@172.30.193.53}
# The two FTDI adapters share vendor:product, so the factory serial is the only
# stable key; /dev/ttyREF only exists when the udev rules are installed.
REF_SERIAL=${REF_SERIAL:-A54LQ9PF}
REF_SUDO_PW=${REF_SUDO_PW:-orangepi}

payload() {
	cat <<'REMOTE'
SUDO="sudo -n"
if ! $SUDO true 2>/dev/null; then SUDO="sudo -S -p ''"; fi
# Reads need root; the payload runs unattended, so the password (when needed)
# comes from the caller rather than a prompt that nothing would answer.
asroot() {
  if [ "$SUDO" = "sudo -n" ]; then sudo -n "$@"; else printf '%s\n' "$SUDO_PW" | sudo -S -p '' "$@"; fi
}
rd() {
  # read one 32-bit word of device memory: devmem, or python3 + mmap.
  a=$1
  if command -v devmem >/dev/null 2>&1; then asroot devmem "$a" 2>/dev/null && return 0; fi
  asroot python3 - "$a" <<'PY' 2>/dev/null
import mmap, os, sys
addr = int(sys.argv[1], 16)
page = os.sysconf("SC_PAGE_SIZE")
base = addr & ~(page - 1)
fd = os.open("/dev/mem", os.O_RDONLY | os.O_SYNC)
try:
    m = mmap.mmap(fd, page, mmap.MAP_SHARED, mmap.PROT_READ, offset=base)
    print("0x%08X" % int.from_bytes(m[addr - base:addr - base + 4], "little"))
finally:
    os.close(fd)
PY
}
row() {
  # label, address
  v=$(rd "$2") || v="<unreadable>"
  printf '%-28s %-10s %s\n' "$1" "$2" "$v"
}
SUDO_PW=${SUDO_PW:-__SUDO_PW__}
echo "== host"; uname -a; cat /proc/device-tree/model 2>/dev/null; echo
echo "== PMU @ 0xfd8d8000 (PD_USB = bit 11, PD_PCIE = bit 7)"
row "pwr_con      (0x150)" 0xfd8d8150
row "mem_pwr      (0x1a4)" 0xfd8d81a4
row "req          (0x110)" 0xfd8d8110
row "idle         (0x120)" 0xfd8d8120
row "status       (0x180)" 0xfd8d8180
echo
echo "== CRU soft-reset / clock gates @ 0xfd7c0000"
echo "   (gate id g -> 0x0800+(g/16)*4, bit g%16; ~bit = clock running)"
row "softrst_con42" 0xfd7c0aa8
row "clkgate_con26 (id 0x1a0-0x1af)" 0xfd7c0868
row "clkgate_con25 (id 0x190-0x19f)" 0xfd7c0864
row "clkgate_con27 (id 0x1b0-0x1bf)" 0xfd7c086c
echo
echo "== VBUS enable gpio (gpio3 pin 15): dr/ddr/ext_port"
row "gpio3_swport_dr" 0xfec40000
row "gpio3_swport_ddr" 0xfec40008
row "gpio3_ext_port" 0xfec40070
echo
echo "== USB2 PHY blocks (phy_sus bit lives at block+0x0c bit11 for usb3 pairs)"
row "usb2phy usb3_0 +0x00" 0xfd5d0000
row "usb2phy usb3_0 +0x0c" 0xfd5d000c
row "usb2phy usb3_1 +0x00" 0xfd5d4000
row "usb2phy usb3_1 +0x0c" 0xfd5d400c
echo
echo "== DWC3: xHCI capability word @base, GSNPSID @base+0xc100"
row "fc000000 caplen/ver" 0xfc000000
row "fc000000 gsnpsid" 0xfc00c100
row "fc400000 caplen/ver" 0xfc400000
row "fc400000 gsnpsid" 0xfc40c100
echo
echo "== EHCI alive check (CAPLENGTH in byte 0)"
row "fc800000 ehci caplen" 0xfc800000
row "fc880000 ehci caplen" 0xfc880000
echo
echo "== kernel view"
dmesg 2>/dev/null | grep -iE 'dwc3|xhci|usb2phy|usb [0-9]' | tail -25
echo "-- lsusb -t"
lsusb -t 2>/dev/null || echo "(lsusb missing)"
echo "-- usb device tree"
for d in /sys/bus/usb/devices/*; do
  [ -f "$d/idVendor" ] || continue
  printf '%s id %s:%s speed %s product "%s"\n' "$(basename "$d")" \
    "$(cat "$d/idVendor" 2>/dev/null)" "$(cat "$d/idProduct" 2>/dev/null)" \
    "$(cat "$d/speed" 2>/dev/null)" "$(cat "$d/product" 2>/dev/null)"
done
echo "== done"
REMOTE
}

if [[ ${1:-} == --payload ]]; then
	payload
	exit 0
fi

PORT=${REF_CONSOLE:-}
if [[ -z $PORT ]]; then
	PORT=$(ssh -o BatchMode=yes "$RIG" \
		"ls /dev/serial/by-id/ 2>/dev/null | grep -m1 '${REF_SERIAL}' |
		 sed 's|.*-> \\.\\./\\.\\./|/dev/|' || true")
fi
if [[ -z ${PORT:-} ]]; then
	PORT=$(ssh -o BatchMode=yes "$RIG" 'ls /dev/ttyREF 2>/dev/null || true')
fi
if [[ -z ${PORT:-} ]]; then
	echo "no reference console found (looked for serial ${REF_SERIAL} and /dev/ttyREF)" >&2
	echo "set REF_CONSOLE=/dev/ttyUSBn to override" >&2
	exit 2
fi
echo "== reference console: $PORT (serial ${REF_SERIAL}) on $RIG"

# One line through the console: base64 keeps quoting out of the picture, and a
# single `sh /tmp/usb-dump.sh` makes the output one contiguous block instead of
# the prompt-per-line chatter a command-at-a-time push produces.
B64=$(payload | sed "s|__SUDO_PW__|${REF_SUDO_PW}|" | base64 -w0)
CMD="printf '%s' '$B64' | base64 -d > /tmp/usb-dump.sh && sh /tmp/usb-dump.sh"

ssh -o BatchMode=yes "$RIG" \
	"~/rkt/rkt --console '$PORT' --baud 1500000 exec --timeout 120 $(printf '%q' "$CMD")"
