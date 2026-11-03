#!/usr/bin/env bash
# Driver subsystem boundary checks (standalone).
#
# Mechanizes the rules from docs/ai/driver-subsystem-boundaries.md that are
# grep-able, plus the exception register's integrity. Everything else in that
# document is a judgement call and stays in review.
#
# Rationale: the boundary rule drifted for a whole refactor series because it
# was prose. The project already learned this lesson for feature combinations
# — check_feature_matrix.sh derives its matrix from Kconfig instead of
# enumerating by hand (docs/driver-framework-review.md §8.3.1: "把覆盖从人肉
# 记忆升级为机制保证是唯一的根治法"). Same treatment here: a rule that is not
# mechanically checked is a rule that is not held.
#
# Usage: scripts/ci/check_driver_boundaries.sh
# Reports every violation, then exits non-zero if there was at least one.
set -euo pipefail
cd "$(dirname "$0")/../.."

BOUNDARIES_DOC="docs/ai/driver-subsystem-boundaries.md"
fail=0

report() {
    echo "!!! $1" >&2
    fail=1
}

# --- scope -----------------------------------------------------------------
# Directories the L0/L1 rules apply to. bus/, firmware/, uevent-netlink/,
# boot-console/, clocksource/, irqchip/, perf/, drivers/host and drivers/init
# are host-side (or boot-order-pinned) by role and are deliberately NOT listed:
# the old flat deny-list flagged them, which trained reviewers to ignore the
# whole document.
RULED_DIRS=(drivers/base drivers/devnode drivers/subsystems drivers/drv)

# Host-kernel crates a ruled directory must not depend on directly.
# kirq is NOT here: its type vocabulary (IrqTrigger/IrqController/IrqDomainId)
# is common and driver-core re-exports it. What is forbidden in drv/** is
# kirq's REGISTRATION API — checked separately as L1c.
HOST_CRATES=(khal memspace kdma ktask kwork kpoll kclk knet driver_host boot_console)

# --- L2: the exception register (data, cross-checked against the doc) ------
# (crate dir, host crate) pairs allowed under L0-b. Format: "<dir> <crate>".
DRV_HOST_ALLOW=(
    # E-02a: the vendored virtio-drivers requires VirtIoHal: 'static as a TYPE
    # parameter, so constructing VirtIoBlkDev<H, T> must name a concrete H.
    # E-02b: ExtensionService::from_services is an as_any downcast, so the
    # caller must name the concrete host extension type — that is precisely
    # what keeps driver-core family-agnostic (L0-a).
    "drivers/drv/virtio driver_host"
    # E-02c: a clock provider driver's job IS registering into the host clock
    # subsystem (of_clk_add_hw_provider does the same in Linux).
    "drivers/drv/clk driver_host"
    "drivers/drv/clk kclk"
    # E-03: the atomic console-write regression test must exercise the real
    # khal console lock. serial-core's non-test sources take PhysAddr/VirtAddr
    # from `memaddr`; the `of` dependency is the `earlycon` feature (boot
    # console bring-up), never the model driver path.
    "drivers/drv/serial/serial-core khal"
    # E-05: the RK3588 PLL lock wait is a µs-scale hardware timed poll
    # (Linux clk-pll.c uses udelay); probe/initcall context cannot sleep.
    # khal::time::busy_wait is the kernel's udelay equivalent.
    "drivers/drv/clk khal"
    # E-05 (phy): same µs-scale timed polls (combphy ready poll, pcie3phy
    # SRAM init, inno-usb2 reset pulse) — hardware timed waits in probe.
    "drivers/drv/phy khal"
    # E-02 (phy): a phy provider driver's job IS registering into the phy
    # subsystem (of_phy_provider_register analogue); registration goes
    # through dev.env().provide::<PhyProvider>, so only the class
    # descriptor names the host, same as drv/clk's ClkProvider.
    "drivers/drv/phy driver_host"
    "drivers/drv/phy kphy"
    # E-05 (net): the r8169 reset sequence polls `CmdReset` for up to 10 ms in
    # probe context (the reference's `rtl_loop_wait_low`), which cannot sleep.
    "drivers/drv/net khal"
    # E-05 (usb): the DWC3 core soft reset polls GCTL.CoreSoftReset and the
    # xHCI stop/reset bits, both self-clearing within microseconds, in probe
    # context where sleeping is not available.
    "drivers/drv/usb khal"
    # E-04: the hwrng completion wait needs a task-context park/wake.
    # subsystems/block::dispatch already wraps the identical primitive as a
    # service; hwrng has exactly one consumer today.
    "drivers/drv/virtio ktask"
    "drivers/drv/virtio kpoll"
)

# Files allowed to import the host capability surface under L1b.
PROVIDER_SURFACE_ALLOW=(
    # E-01: the MSI-X config vector belongs to the TRANSPORT (Linux keeps it
    # in virtio_pci_modern.c), is driven from inside the vendored Transport
    # impl, and has a transport-scoped lifetime. The provider is injected by
    # driver-host, never fetched.
    "drivers/drv/virtio/devices/src/pci.rs"
    "drivers/drv/virtio/devices/src/pci/x86_64.rs"
)

# --- patterns --------------------------------------------------------------
# No word boundaries: `VIRTIO_LEAK` must be caught and `_` is a word
# character, so a trailing `\b` would miss exactly the identifier-shaped
# violations this rule exists for. None of these substrings occurs inside an
# unrelated English or Rust identifier.
#
# `spi` is deliberately ABSENT: it collides with `spin` / `kspin` /
# `SpinNoPreempt`, which are everywhere in this tree. An SPI bus added to
# driver-core is caught by the review checklist in the boundaries doc instead.
FAMILIES='virtio|nvme|ahci|sdhci|mmc|i2c|usb|pl011|ns16550'
PROVIDER_SINGLETON='resource_provider|XKernelResourceProvider|XKernelRuntime|GLOBAL_RUNTIME'
PROVIDER_SURFACE='driver_base::devres::host_api|use driver_base::\{[^}]*host_api'
# Longer alternative first: `try_free_irq\b` would not match
# `try_free_irq_action` because `_` is a word character.
KIRQ_REGISTRATION='kirq::(try_register_shared|try_free_irq_action|try_free_irq|try_map|alloc_msix|free_msix|enable)\b'

in_allow() {
    # in_allow <needle> <list...>
    local needle="$1"
    shift
    local entry
    for entry in "$@"; do
        [ "$entry" = "$needle" ] && return 0
    done
    return 1
}

# ---------------------------------------------------------------------------
# L0-a: driver-core names no device family.
#
# Fan-in rule: every driver shares this crate and its model tests, so a
# family-specific addition here risks all of them. Matches are tolerated only
# on comment lines (the crate legitimately discusses virtio in its
# ExtensionService docs); anything else is a violation.
# ---------------------------------------------------------------------------
while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    text="${rest#*:}"
    # A comment line: //, ///, //!, /*, or a block-comment continuation " * ".
    if printf '%s' "$text" | grep -qE '^[[:space:]]*(//|/\*|\*)'; then
        continue
    fi
    report "L0-a: driver-core names a device family at ${file}:${lineno}:${text}"
done < <(grep -rniE "$FAMILIES" drivers/base/src --include='*.rs' || true)

# ---------------------------------------------------------------------------
# L0-b: contracts are host-free (direct Cargo dependencies).
#
# This is what makes MockServices possible — the model and the class layer run
# on mocks, which is why driver-core has state-machine tests at all.
#
# Note: devnode (`drivers/devnode`) takes the log/spin/sync utility crates plus
# `driver-base`, and the latter only for the opaque `PublicationId` value type
# that keys its registry — no bus, host or kernel-facility dependency. The
# reverse direction stays forbidden: the model core must not depend on node
# policy. Its netlink half lives in `drivers/uevent-netlink`, which is why the
# model itself never reaches `knet`. Only DIRECT deps are checked here.
# ---------------------------------------------------------------------------
for dir in "${RULED_DIRS[@]}"; do
    while IFS= read -r manifest; do
        [ -z "$manifest" ] && continue
        crate_dir="$(dirname "$manifest")"
        for host in "${HOST_CRATES[@]}"; do
            grep -qE "^${host}([[:space:]]*=|\.workspace)" "$manifest" || continue
            # subsystems/** CONTRACTS must stay host-free: that is what lets
            # the class layer and mock domains compile without a host. The
            # host-side SERVICES live in their own `*-host` crates
            # (`subsys-block-host` today), which are allowed to use the host.
            case "$crate_dir" in
                drivers/subsystems/*-host) continue ;;
            esac
            if in_allow "${crate_dir} ${host}" "${DRV_HOST_ALLOW[@]}"; then
                continue
            fi
            report "L0-b/L2: ${crate_dir} depends on host crate \`${host}\` and is not in DRV_HOST_ALLOW — register it as E-0x in ${BOUNDARIES_DOC}"
        done
    done < <(find "$dir" -name Cargo.toml 2>/dev/null | sort)
done

# ---------------------------------------------------------------------------
# L1: resources have exactly one acquisition path.
#
# (a) No ruled crate may FETCH a provider — it may only be injected. This is
#     the mechanically checkable half of "one door": a second acquisition path
#     is a resource whose lifetime the Data drop order does not describe.
# (b) No ruled crate may import the provider contract surface, except E-01.
# (c) No drv/** crate may call a kirq REGISTRATION function. The type
#     vocabulary is fine; registration must go through DeviceCtx::request_irq
#     so the DevIrq guard lands in the driver's Data.
# ---------------------------------------------------------------------------
for dir in "${RULED_DIRS[@]}"; do
    while IFS= read -r hit; do
        [ -z "$hit" ] && continue
        file="${hit%%:*}"
        rest="${hit#*:}"
        lineno="${rest%%:*}"
        text="${rest#*:}"
        report "L1a: ${file}:${lineno} names a provider singleton (a provider may be injected, never fetched):${text}"
    done < <(grep -rnE "$PROVIDER_SINGLETON" "$dir" --include='*.rs' || true)
done

for dir in "${RULED_DIRS[@]}"; do
    while IFS= read -r hit; do
        [ -z "$hit" ] && continue
        file="${hit%%:*}"
        rest="${hit#*:}"
        lineno="${rest%%:*}"
        text="${rest#*:}"
        if in_allow "$file" "${PROVIDER_SURFACE_ALLOW[@]}"; then
            continue
        fi
        report "L1b: ${file}:${lineno} reaches around Device into the host capability surface:${text}"
    done < <(grep -rnE "$PROVIDER_SURFACE" "$dir" --include='*.rs' || true)
done

while IFS= read -r hit; do
    [ -z "$hit" ] && continue
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    text="${rest#*:}"
    report "L1c: ${file}:${lineno} calls a kirq registration API; use Device::request_irq / resolve_irq:${text}"
done < <(grep -rnE "$KIRQ_REGISTRATION" drivers/drv --include='*.rs' || true)

# ---------------------------------------------------------------------------
# L2 integrity: the register must match both the doc and the code.
#
# An exception that is not written down with a reason and an exit condition is
# not an exception, it is drift. And an allow-list entry whose code is gone is
# a register that outlived what it excuses.
# ---------------------------------------------------------------------------
for id in E-01 E-02 E-03 E-04; do
    grep -q "| ${id} |" "$BOUNDARIES_DOC" ||
        report "L2: exception ${id} is referenced by this script but not registered in ${BOUNDARIES_DOC}"
done

for entry in "${PROVIDER_SURFACE_ALLOW[@]}"; do
    if [ ! -f "$entry" ]; then
        report "L2: PROVIDER_SURFACE_ALLOW entry ${entry} no longer exists — drop it and its register row"
    elif ! grep -qE "$PROVIDER_SURFACE" "$entry"; then
        report "L2: PROVIDER_SURFACE_ALLOW entry ${entry} no longer needs the exception — drop it and its register row"
    fi
done

for entry in "${DRV_HOST_ALLOW[@]}"; do
    dir="${entry% *}"
    host="${entry#* }"
    if [ ! -f "${dir}/Cargo.toml" ]; then
        report "L2: DRV_HOST_ALLOW entry \`${entry}\` has no Cargo.toml — drop it and its register row"
    elif ! grep -qE "^${host}([[:space:]]*=|\.workspace)" "${dir}/Cargo.toml"; then
        report "L2: DRV_HOST_ALLOW entry \`${entry}\` is no longer a dependency — drop it and its register row"
    fi
done

# ---------------------------------------------------------------------------
# Doc/code consistency: the crates the normative docs name must exist, and the
# pre-rename names must not come back. A rename that does not update the docs
# is exactly the drift this script exists to prevent.
# ---------------------------------------------------------------------------
for crate in driver-base driver-host driver-init devnode; do
    grep -rq "^name = \"${crate}\"" drivers --include=Cargo.toml ||
        report "doc-sync: no crate named \`${crate}\` exists, but the normative docs name it"
done
for stale in driver-runtime kclass driver-core driver-compose device-class device-traits; do
    if grep -rqw "$stale" "$BOUNDARIES_DOC" drivers/README.md AGENTS.md 2>/dev/null; then
        report "doc-sync: \`${stale}\` is a pre-rename crate name still present in ${BOUNDARIES_DOC} / drivers/README.md / AGENTS.md"
    fi
done
# Directory-level counterpart: the two relation-named groupings that held devnode
# and uevent-netlink are gone. Re-creating one puts a layer of naming back
# between a crate and its own name.
for stale_dir in drivers/common drivers/integration; do
    if [ -d "$stale_dir" ]; then
        report "doc-sync: \`${stale_dir}\` was re-created; \`devnode\` and \`uevent-netlink\` are top-level crates under drivers/"
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "" >&2
    echo "driver boundaries: VIOLATIONS above." >&2
    echo "Fix the code, or register the exception in ${BOUNDARIES_DOC} (with a" >&2
    echo "reason and an exit condition) and add it to the allow-list here." >&2
    exit 1
fi
echo "driver boundaries: L0-a (family-agnostic core), L0-b (host-free contracts),"
echo "  L1 (one resource acquisition path) and the E-01..E-05 register all hold."
