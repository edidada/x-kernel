#!/usr/bin/env python3
"""Generate the RK3588 CRU declarative clock table for x-kernel from the
pinned Linux reference tree.

Data domains (see docs/ai/skills/linux-driver-port/examples/rk3588-cru/portrait.md):
  - registers / bits / parents / flags / sequences:
      data/references/linux/drivers/clk/rockchip/clk-rk3588.c  (+ clk.h macros)
  - DT binding indices (what `clocks = <&cru N>` consumers use):
      data/references/bsp-opi5plus/include/dt-bindings/clock/rockchip,rk3588-cru.h
  - mainline indices (fallback for clocks absent from the BSP header):
      data/references/linux/include/dt-bindings/clock/rockchip,rk3588-cru.h

Run from the repo root:  python3 scripts/port-rk3588-cru/generate.py
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REF = ROOT / "docs/ai/skills/linux-driver-port/data/references"
DRV = REF / "linux/drivers/clk/rockchip/clk-rk3588.c"
CLK_H = REF / "linux/drivers/clk/rockchip/clk.h"
LINUX_IDS = REF / "linux/include/dt-bindings/clock/rockchip,rk3588-cru.h"
BSP_IDS = REF / "bsp-opi5plus/include/dt-bindings/clock/rockchip,rk3588-cru.h"
OUT = ROOT / "drivers/integration/kdriver/src/driver_registry/clk/rk3588_cru/data.rs"

# ---------------------------------------------------------------- parsing --


def parse_defines(path: Path) -> dict:
    text = path.read_text()
    defs = {}
    for line in text.splitlines():
        m = re.match(r"#define\s+([\w]+)(\([^)]*\))?\s+(.+?)(?:\s*$)", line)
        if m:
            name, params, body = m.group(1), m.group(2), m.group(3).strip()
            defs[name] = (params, body)  # params None = object-like constant
    return defs


def eval_offset(expr: str, defines: dict) -> int:
    """Evaluate a rockchip register-offset expression like RK3588_CLKSEL_CON(9)."""
    expr = expr.strip()

    def repl(m):
        name, arg = m.group(1), m.group(2)
        if name not in defines:
            raise KeyError(name)
        params, body = defines[name]
        pnames = [p.strip() for p in (params or "").strip("()").split(",") if p.strip()]
        out = body
        # rockchip offset-macro bodies are fully parenthesized; numeric args
        # substitute verbatim, keeping the expression balanced.
        for pname, a in zip(pnames, [arg]):
            out = re.sub(r"\b" + pname + r"\b", a, out)
        return out

    for _ in range(16):
        new = re.sub(r"(\w+)\(\s*(\d+)\s*\)", repl, expr)
        if new == expr:
            break
        expr = new
    expr = re.sub(r"(\d)U\b", r"\1", expr)  # C unsigned suffix, not the letter U
    # constant macros (no parens left) — resolve identifiers
    for _ in range(8):
        new = re.sub(
            r"\b(\w+)\b",
            lambda m: "(" + defines[m.group(1)][1] + ")"
            if m.group(1) in defines and defines[m.group(1)][0] is None
            else m.group(1),
            expr,
        )
        if new == expr:
            break
        expr = new
    return int(eval(expr, {"__builtins__": {}}))


def parse_id_header(path: Path) -> dict:
    ids = {}
    for m in re.finditer(r"#define\s+([A-Z][A-Z0-9_]*)\s+(\d+)", path.read_text()):
        name, val = m.group(1), int(m.group(2))
        if name.startswith(("SCMI_", "SRST_")):
            continue  # SCMI clocks / reset ids are different provider namespaces
        # first definition wins (the header may redefine guard leftovers)
        ids.setdefault(name, val)
    return ids


ZERO_ID_NAMES: list = []


def lookup_id(cname: str, a0: str, id_map: dict):
    """Binding index for clock `cname`. Linux tables use literal id 0 for
    name-only fillers (sclk_dsu, xin12m, ...); kclk indices must be unique
    and DT never references these by index, so they get unique synthetic
    indices past every header id."""
    key = cname.strip('"').upper()
    if key in id_map:
        return id_map[key]
    ZERO_ID_NAMES.append(cname.strip('"'))
    return None, "zero"


def split_args(s: str):
    """Split top-level comma-separated args (parentheses aware)."""
    args, depth, cur = [], 0, ""
    for ch in s:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        args.append(cur.strip())
    return args


def parse_pnames(src: str) -> dict:
    names = {}
    for m in re.finditer(r"PNAME\((\w+)\)\s*=\s*\{([^}]*)\}", src):
        entries = [e.strip().strip('"') for e in m.group(2).split(",") if e.strip()]
        names[m.group(1)] = entries
    return names


def parse_rate_table(src: str) -> list:
    rates = []
    for m in re.finditer(r"RK3588_PLL_RATE\((\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+)\)", src):
        mhz, p, m, s, k = (int(g) for g in m.groups())
        rates.append((mhz * 1_000_000, p, m, s, k))
    return rates


def parse_array(name: str, src: str):
    """Extract the body of `... name[] ... = { ... };`, comments stripped."""
    m = re.search(re.escape(name) + r"\[\][^=]*=\s*\{", src)
    if not m:
        return None
    depth, i = 1, m.end()
    while depth and i < len(src):
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
        i += 1
    body = src[m.end():i - 1]
    return re.sub(r"/\*.*?\*/", "", body, flags=re.S)


def parse_entries(body: str):
    """Yield (macro, args) for each top-level macro invocation in the body."""
    for m in re.finditer(r"\b([A-Z_0-9]+)\(([^;]*?)\)(?=\s*,?\s*$|\s*;|\s*\n)", body, re.M):
        macro, argstr = m.group(1), m.group(2)
        if macro in MACROS:
            yield macro, split_args(argstr)
    # single invocation per element separated by commas at depth 0:
    # the regex above handles one-per-line style; verify count below.


def parse_entries_robust(body: str):
    entries = []
    depth = brace = 0
    cur = ""
    for ch in body:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "{":
            brace += 1
        elif ch == "}":
            brace -= 1
        if ch == "," and depth == 0 and brace == 0:
            entries.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        entries.append(cur)
    out = []
    for e in entries:
        e = e.strip().rstrip(",").strip()
        m = re.match(r"([A-Z_0-9]+)\((.*)\)$", e, re.S)
        if m and m.group(1) in MACROS:
            out.append((m.group(1), split_args(m.group(2))))
    return out


def parse_standalone_muxes(src: str):
    """`static struct rockchip_clk_branch rk3588_x_fracmux __initdata = MUX(...)`"""
    out = {}
    for m in re.finditer(r"rk3588_(\w+_fracmux)\s+__initdata\s*=\s*([A-Z_0-9]+)\(([^;]*)\);", src, re.S):
        var, macro, argstr = "rk3588_" + m.group(1), m.group(2), m.group(3)
        if macro in MACROS:
            out[var] = (macro, split_args(argstr))
    return out


MACROS = {
    "GATE", "COMPOSITE", "COMPOSITE_NODIV", "COMPOSITE_NOMUX", "COMPOSITE_HALFDIV",
    "COMPOSITE_FRACMUX", "MUX", "DIV", "FACTOR", "GATE_LINK", "MMC",
}

# ---------------------------------------------------------------- emission --

FLAG_MAP = {"CLK_IGNORE_UNUSED": "kclk::ClkFlags::IGNORE_UNUSED",
            "CLK_IS_CRITICAL": "kclk::ClkFlags::IS_CRITICAL",
            "CLK_SET_RATE_PARENT": "kclk::ClkFlags::SET_RATE_PARENT",
            "CLK_SET_RATE_NO_REPARENT": "kclk::ClkFlags::NO_REPARENT"}


def rust_flags(tokens):
    split = [t.strip() for tok in tokens for t in tok.split("|")]
    toks = [FLAG_MAP[t] for t in split if t in FLAG_MAP]
    unknown = [t for t in split if t not in FLAG_MAP and t not in ("0",)]
    if unknown:
        print(f"  WARN unknown branch flags: {unknown}")
    if not toks:
        return "kclk::ClkFlags::empty()"
    out = toks[0]
    for t in toks[1:]:
        out += f".union({t})"
    return out


def parent_list(token, pnames):
    if token.startswith('"'):
        return [token.strip('"')]
    return pnames[token]


def esc(name):
    return name.replace('"', '\\"')


def rq(name: str) -> str:
    """Rust string literal (double-quoted) for a clock name."""
    return '"' + name + '"'



def main():
    src = DRV.read_text()
    clk_h = CLK_H.read_text()
    defines = parse_defines(CLK_H)
    defines.update(parse_defines(DRV))
    linux_ids = parse_id_header(LINUX_IDS)
    bsp_ids = parse_id_header(BSP_IDS)
    pnames = parse_pnames(src)

    early_body = parse_array("rk3588_early_clk_branches", src)
    late_body = parse_array("rk3588_clk_branches", src)
    assert early_body and late_body
    entries = parse_entries_robust(early_body) + parse_entries_robust(late_body)
    branch_entries = entries  # `entries` is rebound by the cpuclk table loop
    fracmux = parse_standalone_muxes(src)
    rates = parse_rate_table(src)

    print(f"branch entries: {len(entries)} (early+late), fracmux defs: {len(fracmux)}, pll rates: {len(rates)}")

    # ---- resolve ids: BSP numbering first, mainline fallback --------------
    # name -> (index, source)
    id_map = {}
    for name, v in bsp_ids.items():
        id_map[name] = (v, "bsp")
    # BSP-internal duplicate ids (vendor header typos): first definition wins,
    # later names get synthetic indices.
    seen_vals = {}
    for name, (v, src_) in list(id_map.items()):
        if src_ != "bsp":
            continue
        if v in seen_vals:
            id_map[name] = (None, "dup")
        else:
            seen_vals[v] = name

    fallback = []
    used = {v for v, _ in id_map.values() if v is not None}
    next_free = max(used) + 1 if used else 0
    assigned = {}
    for name, v in linux_ids.items():
        if name in id_map and id_map[name][0] is not None:
            continue
        if name in id_map and id_map[name][0] is None:
            # dup victim: force synthetic even if mainline value is free
            id_map[name] = (next_free, "synthetic")
            assigned[name] = next_free
            next_free += 1
            continue
        if v in used:
            # mainline-only clock whose mainline id collides with a BSP id
            # (ids >= 701 diverged): give it a free high index. DT consumers
            # built against the BSP header cannot reference it by that id;
            # recorded in the gap report.
            id_map[name] = (next_free, "synthetic")
            assigned[name] = next_free
            next_free += 1
        else:
            id_map[name] = (v, "linux")
            fallback.append(name)
    if assigned:
        print(f"  synthetic indices for mainline-only clocks: {len(assigned)} (first: {sorted(assigned.items(), key=lambda kv: kv[1])[:4]})")

    # pre-pass: literal id-0 filler clocks get unique synthetic indices
    # (Linux uses id 0 for name-only fillers; kclk requires unique indices
    # and DT consumers never reference them by index)
    zero_names = [a[1].strip('"') for macro, a in entries if macro != "MMC" and a[0].strip() == "0"]
    used_ids = {v for v, _ in id_map.values()}
    next_free = max(used_ids) + 1
    for name in zero_names:
        id_map[name.strip('"').upper()] = (next_free, "zero-synthetic")
        next_free += 1
    print(f"  zero-id filler clocks: {len(zero_names)} → synthetic indices {next_free - len(zero_names)}..{next_free - 1}")

    lines = []
    w = lines.append
    w("// SPDX-License-Identifier: Apache-2.0")
    w("// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>")
    w("// See LICENSES for license details.")
    w("")
    w("// @generated by scripts/port-rk3588-cru/generate.py — do not edit by hand.")
    w("//")
    w("// Mechanical translation of the pinned Linux reference:")
    w("//   drivers/clk/rockchip/clk-rk3588.c (early_clk_branches + clk_branches)")
    w("// DT binding indices follow the OrangePi 5 Plus BSP header (the numbering")
    w("// the board DT was compiled with); clocks absent there keep mainline ids.")
    w("#![allow(clippy::too_many_arguments)]")
    w("#![allow(non_upper_case_globals)]")
    w("")
    w("use kclk::{ClkBranch, ClkIndex, DividerDesc, DividerFlags, GateDesc, GateFlags, MuxDesc, MuxFlags};")
    w("")
    # static parent-name lists used by generated entries
    used_pnames = set()

    def emit_pname(token):
        if not token.startswith('"'):
            used_pnames.add(token)
            return token
        return None

    entries_out = []
    skipped_mmc = []
    customs = []  # (kind, id_name, name) handled by the driver's runtime builder
    for macro, a in entries:
        if macro == "GATE":
            (i, cname, pname, f, go, gs, gf) = a
            idx, isrc = lookup_id(cname, i, id_map)
            entries_out.append(f"    ClkBranch::gate({idx}, {cname}, {eval_offset(go, defines)}, {gs}, GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK), P_{parent_list(pname, pnames)[0].upper() if False else ''})"
                               ) if False else None
            gname = parent_list(pname, pnames)
            assert len(gname) == 1
            pn = gname[0]

            parr = pname if not pname.startswith('"') else None
            if parr is None:
                parents = f"&[{rq(pn)}]"
            else:
                used_pnames.add(parr)
                parents = f"&{parr}"
            entries_out.append(
                f"    ClkBranch::gate({idx}, {cname}, {eval_offset(go, defines)}, {gs}, "
                f"GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK), {parents})"
                f".with_flags({rust_flags([f])})"
            )
        elif macro == "COMPOSITE":
            (i, cname, pns, f, mo, ms, mw, mf, ds, dw, df, go, gs, gf) = a
            idx, _ = lookup_id(cname, i, id_map)
            used_pnames.add(pns)
            ro = " | CLK_DIVIDER_READ_ONLY" in df or "CLK_DIVIDER_READ_ONLY" in df
            dflags = ("DividerFlags::HIWORD_MASK.union(DividerFlags::READ_ONLY)") if ro else "DividerFlags::HIWORD_MASK"
            parents = f"&{pns}"
            entries_out.append(
                f"    ClkBranch::composite({idx}, {cname}, "
                f"Some(MuxDesc {{ offset: {eval_offset(mo, defines)}, shift: {ms}, mask: (1u32 << {mw}) - 1, table: None, flags: MuxFlags::HIWORD_MASK }}), "
                f"Some(DividerDesc {{ offset: {eval_offset(mo, defines)}, shift: {ds}, width: {dw}, table: None, flags: {dflags} }}), "
                f"Some(GateDesc {{ offset: {eval_offset(go, defines)}, bit: {gs}, flags: GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK) }}), "
                f"{parents}, {rust_flags([f])})"
            )
        elif macro == "COMPOSITE_NODIV":
            (i, cname, pns, f, mo, ms, mw, mf, go, gs, gf) = a
            idx, _ = lookup_id(cname, i, id_map)
            used_pnames.add(pns)
            entries_out.append(
                f"    ClkBranch::composite({idx}, {cname}, "
                f"Some(MuxDesc {{ offset: {eval_offset(mo, defines)}, shift: {ms}, mask: (1u32 << {mw}) - 1, table: None, flags: MuxFlags::HIWORD_MASK }}), "
                f"None, "
                f"Some(GateDesc {{ offset: {eval_offset(go, defines)}, bit: {gs}, flags: GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK) }}), "
                f"&{pns}, {rust_flags([f])})"
            )
        elif macro == "COMPOSITE_NOMUX":
            (i, cname, pname, f, mo, ds, dw, df, go, gs, gf) = a
            idx, _ = lookup_id(cname, i, id_map)
            ro = "CLK_DIVIDER_READ_ONLY" in df
            dflags = ("DividerFlags::HIWORD_MASK.union(DividerFlags::READ_ONLY)") if ro else "DividerFlags::HIWORD_MASK"
            pn = parent_list(pname, pnames)[0]
            entries_out.append(
                f"    ClkBranch::composite({idx}, {cname}, None, "
                f"Some(DividerDesc {{ offset: {eval_offset(mo, defines)}, shift: {ds}, width: {dw}, table: None, flags: {dflags} }}), "
                f"Some(GateDesc {{ offset: {eval_offset(go, defines)}, bit: {gs}, flags: GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK) }}), "
                f"&[{rq(pn)}], {rust_flags([f])})"
            )
        elif macro == "COMPOSITE_HALFDIV":
            (i, cname, pns, f, mo, ms, mw, mf, ds, dw, df, go, gs, gf) = a
            idx, _ = lookup_id(cname, i, id_map)
            customs.append(("halfdiv", cname, pns, a))
        elif macro == "COMPOSITE_FRACMUX":
            (i, cname, pname, f, mo, df, go, gs, gf, ch) = a
            customs.append(("fracmux", cname, pname, a))
        elif macro == "MUX":
            (i, cname, pns, f, o, s, w_, mf) = a
            idx, _ = lookup_id(cname, i, id_map)
            used_pnames.add(pns)
            entries_out.append(
                f"    ClkBranch::mux({idx}, {cname}, {eval_offset(o, defines)}, {s}, (1u32 << {w_}) - 1, None, "
                f"MuxFlags::HIWORD_MASK, &{pns}).with_flags({rust_flags([f])})"
            )
        elif macro == "DIV":
            (i, cname, pname, f, o, s, w_, df) = a
            idx, _ = lookup_id(cname, i, id_map)
            pn = parent_list(pname, pnames)[0]
            ro = "CLK_DIVIDER_READ_ONLY" in df
            dflags = ("DividerFlags::HIWORD_MASK.union(DividerFlags::READ_ONLY)") if ro else "DividerFlags::HIWORD_MASK"
            entries_out.append(
                f"    ClkBranch::divider({idx}, {cname}, {eval_offset(o, defines)}, {s}, {w_}, None, "
                f"{dflags}, &[{rq(pn)}]).with_flags({rust_flags([f])})"
            )
        elif macro == "FACTOR":
            (i, cname, pname, f, fm, fd) = a
            idx, _ = lookup_id(cname, i, id_map)
            pn = parent_list(pname, pnames)[0]
            entries_out.append(
                f"    ClkBranch::fixed_factor({idx}, {cname}, {fm}, {fd}, &[{rq(pn)}])"
                f".with_flags({rust_flags([f])})"
            )
        elif macro == "GATE_LINK":
            (i, cname, pname, linked, f, o, b, gf) = a
            idx, _ = lookup_id(cname, i, id_map)
            pn = parent_list(pname, pnames)[0]
            entries_out.append(
                f"    ClkBranch::gate({idx}, {cname}, {eval_offset(o, defines)}, {b}, "
                f"GateFlags::SET_TO_DISABLE.union(GateFlags::HIWORD_MASK), &[{rq(pn)}])"
                f".with_flags({rust_flags([f, 'CLK_SET_RATE_PARENT'])})"
            )
            customs.append(("gateline", cname, linked, a))
        elif macro == "MMC":
            skipped_mmc.append(a)
        else:
            print(f"  WARN unhandled macro {macro}")

    print(f"emitted static entries: {len(entries_out)}, customs: {len(customs)} ({[k for k, *_ in customs][:4]}...), skipped MMC: {len(skipped_mmc)}")

    # ---- validate indices --------------------------------------------------
    seen = {}
    for macro, a in entries:
        if macro == "MMC":
            continue
        cname = a[1].strip('"')
        idx, src_ = lookup_id(cname, a[0], id_map)
        if idx is None:
            print(f"  WARN no id for {cname}")
        elif idx in seen:
            print(f"  ERROR duplicate index {idx}: {cname} vs {seen[idx]}")
        else:
            seen[idx] = cname
    print(f"unique indices: {len(seen)}, max index: {max(seen) if seen else 0}")

    # ---- emit --------------------------------------------------------------
    w(f"pub(crate) const ARMCLK_L_INDEX: ClkIndex = {id_map.get('ARMCLK_L', (10, ''))[0]};")
    w(f"pub(crate) const ARMCLK_B01_INDEX: ClkIndex = {id_map.get('ARMCLK_B01', (11, ''))[0]};")
    w(f"pub(crate) const ARMCLK_B23_INDEX: ClkIndex = {id_map.get('ARMCLK_B23', (12, ''))[0]};")
    # customs reference PNAME statics too (frac child mux parents etc.)
    for kind, cname, extra, a in customs:
        if kind == "halfdiv":
            used_pnames.add(a[2])
    for var, (cmacro, cargs) in fracmux.items():
        used_pnames.add(cargs[2])
    w("// Parent-name lists (PNAMEs), verbatim from the reference.")
    used_pnames.discard("mux_pll_p")
    for pn in sorted(used_pnames):
        names = pnames[pn]
        w("#[allow(dead_code)]")
        w(f"pub(crate) static {pn}: [&str; {len(names)}] = [{', '.join(rq(n) for n in names)}];")
    w("")
    w(f"pub(crate) static BRANCHES: [ClkBranch; {len(entries_out)}] = [")
    for e in entries_out:
        w(f"    {e},")
    w("];")
    w("")
    w(f"/// Customs handled by the driver's runtime builder: {sorted(set(k for k, *_ in customs))}")
    w(f"pub(crate) const CUSTOM_COUNT: usize = {len(customs)};")
    w("")
    # customs manifest (data only; the ops wiring lives in the driver)
    w("/// One runtime-built node (frac / half-divider / pll / gate-link glue).")
    w(f"pub(crate) const CUSTOMS: [super::CustomSpec; {len(customs)}] = [")
    for kind, cname, extra, a in customs:
        if kind == "halfdiv":
            (i, cname_, pns, f, mo, ms, mw, mf, ds, dw, df, go, gs, gf) = a
            idx, _ = lookup_id(cname_, i, id_map)
            w(f"    super::CustomSpec {{ kind: super::CustomKind::HalfDiv, index: Some({idx}), name: {cname_}, "
              f"parents: &{pns}, mux: Some(super::Field {{ offset: 0x{eval_offset(mo, defines):x}, shift: {ms}, mask: (1u32 << {mw}) - 1 }}), "
              f"div: Some(super::Field {{ offset: 0x{eval_offset(mo, defines):x}, shift: {ds}, mask: (1u32 << {dw}) - 1 }}), "
              f"gate: Some(super::Field {{ offset: 0x{eval_offset(go, defines):x}, shift: {gs}, mask: 1 }}), "
              f"child: None, link_clock: None, flags: {rust_flags([f])} }},")
        elif kind == "fracmux":
            (i, cname_, pname, f, mo, df, go, gs, gf, ch) = a
            idx, _ = lookup_id(cname_, i, id_map)
            cmacro, cargs = fracmux[ch.strip().lstrip("&")]
            # child mux: MUX(id, cname, pnames, f, o, s, w, mf)
            (ci, cn, cpns, cf, co, cs, cw, cmf) = cargs
            used_pnames.add(cpns)
            cidx = id_map.get(ci, (None, None))[0]
            w(f"    super::CustomSpec {{ kind: super::CustomKind::Frac, index: Some({idx}), name: {cname_}, "
              f"parents: &[{rq(parent_list(pname, pnames)[0])}], mux: None, "
              f"div: Some(super::Field {{ offset: 0x{eval_offset(mo, defines):x}, shift: 0, mask: 0xffff }}), "
              f"gate: Some(super::Field {{ offset: 0x{eval_offset(go, defines):x}, shift: {gs}, mask: 1 }}), "
              f"child: Some(super::FracChild {{ index: {cidx}, name: {cn}, parents: &{cpns}, mux_offset: 0x{eval_offset(co, defines):x}, "
              f"mux_shift: {cs}, mux_mask: (1u32 << {cw}) - 1, frac_parent_index: {pnames[cpns].index(cname_.strip(chr(34)))} }}), "
              f"link_clock: None, flags: {rust_flags([f])} }},")
        elif kind == "gateline":
            (i, cname_, pname, linked, f, o, b, gf) = a
            w(f"    super::CustomSpec {{ kind: super::CustomKind::GateLink, index: None, name: {cname_}, "
              f"parents: &[], mux: None, div: None, gate: None, child: None, "
              f"link_clock: Some({rq(linked.lower())}), flags: {rust_flags([f])} }},")
    w("];")
    w("")

    # ---- cpuclk (armclk) rate tables ---------------------------------------
    # RK3588_CORE_*_SEL macros, expanded per clk-rk3588.c:128-241.
    def hiword(val, mask, shift):
        return (val << shift) | (mask << (shift + 16))

    def sel_b0(a): return (0x50300, hiword(a, 0x3, 13) | hiword(0, 0x1f, 1))
    def sel_b1(a): return (0x50304, hiword(a, 0x3, 5))
    def sel_b2(a): return (0x52300, hiword(a, 0x3, 13) | hiword(0, 0x1f, 1))
    def sel_b3(a): return (0x52304, hiword(a, 0x3, 5))
    def sel_l0(offs, a): return (0x58300 + 4 * (6 + offs), hiword(a, 0x3, 5) | hiword(a, 0x3, 12))
    def sel_l1(seldsu, divdsu): return (0x58300, hiword(seldsu, 0x3, 12) | hiword(divdsu - 1, 0x1f, 7))
    def sel_l2(m, mp, s): return (0x58304, hiword(m - 1, 0x1f, 1) | hiword(mp - 1, 0x1f, 11) | hiword(s - 1, 0x1f, 6))
    def sel_l3(per): return (0x58308, hiword(per - 1, 0x1f, 0))
    def sel_l4(g, at): return (0x5830c, hiword(g - 1, 0x1f, 5) | hiword(at - 1, 0x1f, 0))

    def emit_sel(sel):
        reg, val = sel
        return f"ClkSel {{ reg: 0x{reg:x}, val: 0x{val:08x} }}"

    cpub = parse_array("rk3588_cpub0clk_rates", src) or ""
    entries_cpub0 = [(int(a[0]), int(a[1])) for a in
                     (split_args(x) for x in re.findall(r"RK3588_CPUB01CLK_RATE\(([^)]*)\)", cpub))]
    cpub1_body = parse_array("rk3588_cpub1clk_rates", src) or ""
    entries_cpub1 = [(int(a[0]), int(a[1])) for a in
                     (split_args(x) for x in re.findall(r"RK3588_CPUB23CLK_RATE\(([^)]*)\)", cpub1_body))]
    cpul_body = parse_array("rk3588_cpulclk_rates", src) or ""
    entries_cpul = [(int(a[0]), int(a[1]), int(a[2]), int(a[3])) for a in
                    (split_args(x) for x in re.findall(r"RK3588_CPULCLK_RATE\(([^)]*)\)", cpul_body))]
    print(f"cpuclk rate entries: cpub0={len(entries_cpub0)} cpub1={len(entries_cpub1)} cpul={len(entries_cpul)}")

    w("// ---- armclk (cpuclk) data — clk-cpu.c rockchip_cpuclk_rate_table ----")
    w("pub(crate) struct ClkSel {")
    w("    pub reg: usize,")
    w("    pub val: u32,")
    w("}")
    w("")
    w("pub(crate) struct CpuRate {")
    w("    pub prate: u64,")
    w("    pub pre: &'static [ClkSel],")
    w("    pub post: &'static [ClkSel],")
    w("}")
    w("")
    for tag, entries, pre, post in (
        ("B01", entries_cpub0, lambda e: [sel_b0(0), sel_b1(0)], lambda e: [sel_b0(e[1]), sel_b1(e[1])]),
        ("B23", entries_cpub1, lambda e: [sel_b2(0), sel_b3(0)], lambda e: [sel_b2(e[1]), sel_b3(e[1])]),
        ("L", entries_cpul,
         lambda e: [sel_l0(0, 0), sel_l0(1, 0), sel_l1(3, 2), sel_l2(2, 3, 3), sel_l3(4), sel_l4(4, 4)],
         lambda e: [sel_l0(0, e[1]), sel_l0(1, e[1]), sel_l1(e[2], e[3])]),
    ):
        w(f"pub(crate) static CPU{tag}_RATES: [CpuRate; {len(entries)}] = [")
        for e in entries:
            w(f"    CpuRate {{ prate: {e[0]}, pre: &[{', '.join(emit_sel(x) for x in pre(e))}], "
              f"post: &[{', '.join(emit_sel(x) for x in post(e))}] }},")
        w("];")
        w("")
    # armclk reg data
    w("pub(crate) struct CoreDiv {")
    w("    pub reg: usize,")
    w("    pub shift: u8,")
    w("    pub mask: u32,")
    w("}")
    w("")
    w("pub(crate) struct CpuRegData {")
    w("    pub core: &'static [CoreDiv],")
    w("    pub mux_reg: Option<usize>,")
    w("    pub mux_alt: u32,")
    w("    pub mux_main: u32,")
    w("    pub mux_shift: u8,")
    w("    pub mux_mask: u32,")
    w("    pub main_parent: &'static str,")
    w("    pub alt_parent: &'static str,")
    w("}")
    w("")
    dsu = 0x58300
    w("pub(crate) const CPU_B01_DATA: CpuRegData = CpuRegData {")
    w("    core: &[CoreDiv { reg: 0x50300, shift: 8, mask: 0x1f }, CoreDiv { reg: 0x50304, shift: 0, mask: 0x1f }],")
    w("    mux_reg: None, mux_alt: 1, mux_main: 2, mux_shift: 6, mux_mask: 0x3,")
    w('    main_parent: "b0pll", alt_parent: "gpll",')
    w("};")
    w("")
    w("pub(crate) const CPU_B23_DATA: CpuRegData = CpuRegData {")
    w("    core: &[CoreDiv { reg: 0x52300, shift: 8, mask: 0x1f }, CoreDiv { reg: 0x52304, shift: 0, mask: 0x1f }],")
    w("    mux_reg: None, mux_alt: 1, mux_main: 2, mux_shift: 6, mux_mask: 0x3,")
    w('    main_parent: "b1pll", alt_parent: "gpll",')
    w("};")
    w("")
    w("pub(crate) const CPU_L_DATA: CpuRegData = CpuRegData {")
    w(f"    core: &[CoreDiv {{ reg: 0x{dsu + 24:x}, shift: 0, mask: 0x1f }}, CoreDiv {{ reg: 0x{dsu + 24:x}, shift: 7, mask: 0x1f }},"
      f" CoreDiv {{ reg: 0x{dsu + 28:x}, shift: 0, mask: 0x1f }}, CoreDiv {{ reg: 0x{dsu + 28:x}, shift: 7, mask: 0x1f }}],")
    w(f"    mux_reg: Some(0x{dsu + 20:x}), mux_alt: 1, mux_main: 2, mux_shift: 14, mux_mask: 0x3,")
    w('    main_parent: "lpll", alt_parent: "gpll",')
    w("};")
    w("")

    # PLLs
    pll_body = parse_array("rk3588_pll_clks", src) + "\n};"
    plls = []
    for m in re.finditer(r"\[(\w+)\]\s*=\s*PLL\((.*?)\)\s*(?=,\s*\[|,\s*\};|\s*\};)", pll_body, re.S):
        enum_name, args = m.group(1), split_args(m.group(2))
        (ptype, pid, pname_, pnames_, bflags, con, mode, mshift, lshift, pllflags, rtable) = args
        # PLL binding ids are named PLL_<NAME> in the binding header
        key = "PLL_" + pname_.strip('"').upper()
        idx = id_map.get(key, id_map.get(pname_.strip('"').upper(), (None, None)))[0]
        plls.append((idx, pname_, ptype, eval_offset(con, defines), eval_offset(mode, defines), mshift, bflags, pnames_))
    if len(plls) != 9:
        print(f"  WARN expected 9 plls, got {len(plls)}")
    used_pnames.add("mux_pll_p")
    pll_parents_static = pnames["mux_pll_p"]
    w("#[allow(dead_code)]")
    w(f"pub(crate) static mux_pll_p: [&str; {len(pll_parents_static)}] = [{', '.join(rq(n) for n in pll_parents_static)}];")
    w(f"pub(crate) const PLL_COUNT: usize = {len(plls)};")
    w("pub(crate) const PLLS: [super::PllSpec; PLL_COUNT] = [")
    for idx, name, ptype, con, mode, mshift, pflags, pnames_ in plls:
        kind = "Core" if "core" in ptype else "Normal"
        w(f"    super::PllSpec {{ index: {idx}, name: {name}, pll_name: concat!(\"pll_\", {name}), kind: super::PllKind::{kind}, "
          f"con_offset: 0x{con:x}, mode_offset: 0x{mode:x}, mode_shift: {mshift}, "
          f"flags: {rust_flags([pflags])}, parents: &{pnames_} }},")
    w("];")
    w("")
    # PLL rate table
    w(f"pub(crate) const PLL_RATES: [super::PllRate; {len(rates)}] = [")
    for (hz, p, m, s, k) in rates:
        w(f"    super::PllRate {{ rate: {hz}, p: {p}, m: {m}, s: {s}, k: {k} }},")
    w("];")
    # ---- external orphan parents (referenced but never defined here) ------
    defined = set()
    for macro, a in branch_entries:
        if macro != "MMC":
            defined.add(a[1].strip('"'))
    # customs define their own outputs (frac child muxes, half-dividers, ...)
    for kind, cname, extra, a in customs:
        defined.add(cname.strip('"'))
        if kind == "fracmux":
            ch_var = a[9].strip().lstrip("&")
            cmacro, cargs = fracmux[ch_var]
            defined.add(cargs[1].strip('"'))
    # PLL mode muxes + internal pll nodes
    for idx, pname_, ptype, con, mode, mshift, bflags, pnames_ in plls:
        defined.add(pname_.strip('"'))
        defined.add("pll_" + pname_.strip('"'))
    defined.update({"armclk_l", "armclk_b01", "armclk_b23"})
    dt_fixed = {"xin24m", "xin32k", "spll"}  # separate fixed-clock DT providers
    referenced = set()
    for pn in used_pnames:
        referenced.update(pnames[pn])
    for macro, a in branch_entries:
        if macro in ("GATE", "DIV", "FACTOR", "COMPOSITE_NOMUX", "COMPOSITE_FRACMUX"):
            referenced.add(a[2].strip('"'))
        elif macro == "GATE_LINK":
            referenced.add(a[2].strip('"'))
    orphans = sorted(referenced - defined - dt_fixed)
    print(f"external orphan parents ({len(orphans)}): {orphans}")
    w(f"/// External parent clocks with no provider in this kernel (Linux")
    w(f"/// registers them as orphans). The CRU probe publishes zero-rate")
    w(f"/// placeholders so dependent subtrees resolve exactly like the")
    w(f"/// reference; a future provider takes the names over.")
    w(f"pub(crate) const ORPHAN_PARENTS: [&str; {len(orphans)}] = [")
    for o in orphans:
        w(f'    "{o}",')
    w("];")
    w("")
    OUT.write_text("\n".join(lines) + "\n")
    print(f"wrote {OUT} ({len(lines)} lines)")


if __name__ == "__main__":
    sys.exit(main())
