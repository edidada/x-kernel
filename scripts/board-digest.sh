#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# One-screen digest of a board run's console log.
#
# Why: a bring-up console log is thousands of lines of driver tracing, and the two
# questions that decide the next step -- "which commit was this, and did userland
# come up?" -- are one grep each, in a file nobody greps the same way twice. Making
# the digest mechanical also makes commit-by-commit bisecting mechanical: the same
# numbers are printed for every run, so they can be compared across runs.
#
# Usage:
#   scripts/board-digest.sh                 # newest run on the rig
#   scripts/board-digest.sh <tag>           # newest run whose directory matches <tag>
#   scripts/board-digest.sh -f <console.log>
#
# The log is read on the rig when a tag is given (RIG=kylin@172.30.193.53), so this
# works without fetching anything first.
set -euo pipefail

RIG=${RIG:-kylin@172.30.193.53}

usage() {
    echo "usage: $0 [<tag>|-f <console.log>]" >&2
    exit 2
}

log=""
if [[ ${1:-} == -f ]]; then
    [[ -n ${2:-} ]] || usage
    log=$2
else
    tag=${1:-}
    pattern="*${tag}*"
    remote=$(ssh "$RIG" "ls -td ~/rkt/logs/${pattern} 2>/dev/null | head -1" | tr -d '\r' || true)
    if [[ -z $remote ]]; then
        echo "no run matching ~/rkt/logs/${pattern} on $RIG" >&2
        exit 1
    fi
    tmp=$(mktemp)
    ssh "$RIG" "cat '$remote/console.log'" >"$tmp"
    echo "run    : $remote"
    log=$tmp
fi

count() { grep -c -- "$1" "$log" 2>/dev/null || true; }

echo "commit : $(grep -m1 'git_commit' "$log" | sed 's/.*git_commit = //;s/ .*//')"
echo "dirty  : $(grep -m1 'git_dirty' "$log" | sed 's/.*git_dirty = //;s/ .*//')"
echo "build  : $(grep -m1 'build_time' "$log" | sed 's/.*build_time = //;s/ .*//')"
echo
printf 'userland  Enter user space : %s\n' "$(count 'Enter user space')"
printf 'panic     Failed to resolve: %s  (panicked: %s)\n' \
    "$(count 'Failed to resolve')" "$(count 'panicked')"
printf 'mounts    mounted at       : %s\n' "$(count 'mounted at')"
printf 'usb       published device : %s\n' "$(count 'published ') "
printf 'usb       attached         : %s\n' "$(count ' attached (')"
printf 'failures  no completion    : %s\n' "$(count 'no completion')"
printf 'failures  bulk timeout     : %s\n' "$(count 'bulk timeout')"
printf 'failures  probe as * fail  : %s\n' "$(count 'failed:')"
echo
echo "--- the lines that usually decide the next step ---"
grep -E 'mounted at|published .* as |Failed to resolve|panicked|no completion|bulk timeout: ep|does not answer|probe as .* failed' "$log" |
    tail -12 | sed 's/^/  /' || true

[[ -n ${tmp:-} ]] && rm -f "$tmp"
exit 0
