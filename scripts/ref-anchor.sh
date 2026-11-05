#!/usr/bin/env bash
# Reference anchor: read the pinned reference before changing driver code.
#
# Usage:
#   scripts/ref-anchor.sh <reference-file> <function-or-symbol> [note...]
#   scripts/ref-anchor.sh --check          # gate used by hooks / board-run.sh
#
# The first form prints the reference function, so the code being aligned is in
# front of the reader rather than in memory, and appends an entry to
# docs/ai/skills/linux-driver-port/reference-anchors.md. The second form exits
# non-zero when driver code has changed without a fresh anchor entry.
#
# Why this exists: AGENTS.md tells agents to transcribe the reference instead of
# reconstructing it from recall, and a long debugging session loses that
# instruction between rounds. The EHCI bring-up cost seven wrong mechanisms that
# way -- a queue head holding the transfer in the reclamation-list head, MULT=0
# in a high-speed queue head, 0 instead of LIST_END, next|LIST_END terminating
# every element, one shared buffer pointer, completion polled on the overlay,
# and a reset hold that was never held -- while every register bit was correct.
# The instruction was not enough; this makes it a step that cannot be skipped.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$repo_root"

anchors="docs/ai/skills/linux-driver-port/reference-anchors.md"
driver_globs=('drivers/drv/*' 'drivers/bus/*' 'drivers/base/*' 'drivers/host/*' 'drivers/init/*')

# Directories whose changes require an anchor entry.
changed_driver_paths() {
	{
		git diff --name-only --diff-filter=ACM 2>/dev/null || true
		git diff --cached --name-only --diff-filter=ACM 2>/dev/null || true
	} | sort -u | grep -E "^($(IFS='|'; echo "${driver_globs[*]}" | sed 's#\*#[^/]*#g'))" || true
}

if [ "${1:-}" = "--check" ]; then
	changed="$(changed_driver_paths)"
	if [ -z "$changed" ]; then
		exit 0
	fi
	# An anchor entry counts as fresh when the anchors file is also modified in
	# this working tree or index.
	# `git status` rather than `git diff`: a brand-new anchors file is untracked
	# and would otherwise look unmodified.
	if [ -z "$(git status --porcelain -- "$anchors" 2>/dev/null)" ]; then
		echo "❌ ref-anchor: driver code changed without reading the reference." >&2
		echo "   changed: $(echo "$changed" | tr '\n' ' ')" >&2
		echo "   Read the reference function you are aligning, then record it:" >&2
		echo "     scripts/ref-anchor.sh /workspace/linux-7.1.8/drivers/... <function>" >&2
		echo "   (AGENTS.md: bit definitions are the easy half, mechanisms are the" >&2
		echo "    half that breaks; diff structure and order, not just values.)" >&2
		exit 1
	fi
	exit 0
fi

if [ "$#" -lt 2 ]; then
	sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
	exit 2
fi

ref_file="$1"
symbol="$2"
shift 2
note="${*:-}"

if [ ! -f "$ref_file" ]; then
	echo "ref-anchor: no such reference file: $ref_file" >&2
	exit 2
fi

echo "=== $ref_file : $symbol ==="
# Print the function body if we can find one, else every line mentioning the symbol.
if grep -qE "^[a-zA-Z_].*[ *]$symbol\(" "$ref_file" 2>/dev/null; then
	awk -v sym="$symbol" '
		$0 ~ ("^[a-zA-Z_].*[ *]" sym "\\(") { print; started = 1; depth = 0 }
		started {
			n = gsub(/\{/, "{"); depth += n
			m = gsub(/\}/, "}"); depth -= m
			if (started && NR > 1 && depth <= 0 && (n > 0 || m > 0)) { exit }
			if (started) print
		}
	' "$ref_file"
else
	grep -n -- "$symbol" "$ref_file" | head -40
fi
echo "=== end $symbol ==="

mkdir -p "$(dirname "$anchors")"
if [ ! -f "$anchors" ]; then
	cat >"$anchors" <<'HEADER'
# Reference anchors

Every entry is one `scripts/ref-anchor.sh` run: the reference function that was
read before the driver code it aligns. `scripts/ref-anchor.sh --check` refuses to
run the board or to commit driver changes without a fresh entry, so this table
is the record of which mechanisms were transcribed rather than reconstructed.

| when (UTC) | reference | symbol | note |
|---|---|---|---|
HEADER
fi

printf '| %s | `%s` | `%s` | %s |\n' \
	"$(date -u '+%Y-%m-%d %H:%M')" "$ref_file" "$symbol" "${note:--}" >>"$anchors"
echo "recorded in $anchors"