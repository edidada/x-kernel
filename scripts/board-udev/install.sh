#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# Install the rig's stable serial names (see 99-xkernel-rk3588.rules).
#
#   scripts/board-udev/install.sh              # install on the rig over ssh
#   scripts/board-udev/install.sh --local      # install on this host
#   scripts/board-udev/install.sh --patch-rkt  # then aim rkt at the names
#
# The rule install needs root on the target, so over ssh it prompts for the
# rig host's sudo password. (Do not automate that guess: the account locks
# after a few wrong attempts.)
#
# Idempotent: copies the rule, reloads udev, re-triggers the tty subsystem,
# then prints the resulting symlinks. Exits non-zero if any of the three
# stable names is missing, so a wrong serial number surfaces immediately
# instead of as a confusing "port not found" during a board run.

set -euo pipefail

RIG=${RIG:-kylin@172.30.193.53}
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
RULE=99-xkernel-rk3588.rules
EXPECT=(/dev/ttyXK /dev/ttyREF /dev/ttyPWR)

report() {
    local missing=0
    for dev in "${EXPECT[@]}"; do
        if [[ -e $dev ]]; then
            printf '  ok      %s -> %s\n' "$dev" "$(readlink -f "$dev")"
        else
            printf '  MISSING %s\n' "$dev"
            missing=1
        fi
    done
    if ((missing)); then
        cat <<'EOM'
One or more stable names are missing. The match key did not fit the
hardware: check the adapters actually plugged in with
  ls -l /dev/serial/by-id/
and update ATTRS{serial} in the rule accordingly (serials are printed on
the FTDI shells).
EOM
        return 1
    fi
}

patch_rkt() {
    # Point rkt's profile at the stable names. Only after the symlinks
    # exist: rkt fails hard on a missing port, so patching first would break
    # the loop. Run as the rig user (no root needed).
    local profile=~/rkt/profiles/opi5plus.ini
    ssh -o BatchMode=yes "$RIG" "
        set -e
        profile=\$HOME/rkt/profiles/opi5plus.ini
        [ -f \"\$profile\" ] || { echo 'no profile: '\$profile; exit 1; }
        cp \"\$profile\" \"\$profile.bak.\$(date +%Y%m%d%H%M%S)\"
        sed -i 's|^port = /dev/ttyUSB[0-9]*|port = /dev/ttyXK|' \"\$profile\"
        # the [power] section is the second 'port =' line; fix by section
        python3 - \"\$profile\" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path).read()
text = re.sub(r'(\[power\][^[]*?port = )\S+', r'\g<1>/dev/ttyPWR', text, flags=re.S)
open(path, 'w').write(text)
PY
        grep -nE '^port = ' \"\$profile\""
    # NOTE: rkt's --console/--power-port CLI override hit a harness bug
    # ('Console' object has no attribute '_start_wall') on this rig, so the
    # profile is the supported way to aim it at the stable names.
}

if [[ ${1:-} == --patch-rkt ]]; then
    patch_rkt
    exit
fi

if [[ ${1:-} == --local ]]; then
    sudo install -m 0644 "$HERE/$RULE" "/etc/udev/rules.d/$RULE"
    sudo udevadm control --reload
    sudo udevadm trigger --subsystem-match=tty
    sleep 1
    report
else
    ssh -o BatchMode=yes "$RIG" "cat > /tmp/$RULE" <"$HERE/$RULE"
    ssh -o BatchMode=yes "$RIG" \
        "sudo -S -p '' install -m 0644 /tmp/$RULE /etc/udev/rules.d/$RULE \
         && sudo -S -p '' udevadm control --reload \
         && sudo -S -p '' udevadm trigger --subsystem-match=tty" \
        </dev/null
    sleep 1
    ssh -o BatchMode=yes "$RIG" "
        missing=0
        for dev in ${EXPECT[*]}; do
            if [ -e \"\$dev\" ]; then
                printf '  ok      %s -> %s\n' \"\$dev\" \"\$(readlink -f \"\$dev\")\"
            else
                printf '  MISSING %s\n' \"\$dev\"; missing=1
            fi
        done
        exit \$missing"
fi