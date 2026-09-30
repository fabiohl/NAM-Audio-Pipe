#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
"""Filter a full objdump disassembly into a critical-DSP hotspot report.

Reads the complete disassembly (``--full``), keeps only ``.text`` /
``.text.cold`` sections (excluding pre-BOLT ``.bolt.org.text`` duplicates)
and only symbols matching ``--symbols``, writes the filtered report
(``--filtered``) plus a per-symbol summary (``--summary``), and enforces
static gates: zero ``zmm``, zero ``libm`` calls on the RT path, zero
``alloc`` / ``__rust_alloc`` calls.

Exit 0 when all gates pass, 1 when any gate fails (fail-closed).
"""

from __future__ import annotations

import argparse
import re
import sys


SECTION_RE = re.compile(r"^\s*Disassembly of section (\S+?):\s*$")
SYMBOL_RE = re.compile(r"^([0-9a-fA-F]+)\s+<(.+)>:\s*$")
INSTR_RE = re.compile(r"^\s+[0-9a-fA-F]+:\s+")
VFMADD_RE = re.compile(r"vfmadd", re.IGNORECASE)
CALL_RE = re.compile(r"\bcallq?\b")
SPILL_RE = re.compile(r"\(%rsp\)")
ZMM_RE = re.compile(r"zmm\d+", re.IGNORECASE)
# Bare libm symbols only (``<tanhf@GLIBC_...>``); namespaced Rust symbols
# such as ``<...::tanh_and_...>`` never match because ``<`` must directly
# precede the libm name.
LIBM_RE = re.compile(
    r"call\w*\s+.*<(tanhf?|expf?|logf?|powf?|sinf?|cosf?|atan2?f?|"
    r"atanf?|sqrtf?|fmodf?|sincosf?)(@|>)"
)
ALLOC_RE = re.compile(
    r"call\w*\s+.*(__rust_alloc|__rust_dealloc|__rust_realloc|__rg_alloc|<alloc::)"
)


def is_kept_section(section: str) -> bool:
    """Accept only ``.text`` and ``.text.cold*`` (reject pre-BOLT duplicates)."""
    return section == ".text" or section.startswith(".text.cold")


def parse_args() -> argparse.Namespace:
    """Parse the filter CLI (paths plus hotspot symbol regex)."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", required=True, help="Full objdump dump input")
    parser.add_argument("--filtered", required=True, help="Filtered hotspot output")
    parser.add_argument("--summary", required=True, help="Per-symbol summary output")
    parser.add_argument(
        "--symbols",
        required=True,
        help="Python regex selecting hotspot symbols",
    )
    return parser.parse_args()


def main() -> int:
    """Filter, summarize and gate the disassembly; return 0 on pass, 1 on fail."""
    args = parse_args()
    try:
        hotspot = re.compile(args.symbols)
    except re.error as exc:
        print(f"[FATAL] invalid hotspot regex: {exc}", file=sys.stderr)
        return 1

    try:
        with open(args.full, "r", encoding="utf-8", errors="replace") as handle:
            lines = handle.readlines()
    except OSError as exc:
        print(f"[FATAL] cannot read full dump {args.full}: {exc}", file=sys.stderr)
        return 1

    kept: list[str] = []
    stats: dict[str, dict[str, int]] = {}
    cur_section = ""
    cur_symbol: str | None = None
    cur_keep = False
    zmm_hits: list[str] = []
    libm_hits: list[str] = []
    alloc_hits: list[str] = []

    for raw in lines:
        line = raw.rstrip("\n")
        section_match = SECTION_RE.match(line)
        if section_match:
            cur_section = section_match.group(1)
            cur_symbol = None
            cur_keep = False
            if is_kept_section(cur_section):
                kept.append(raw)
            continue
        if not is_kept_section(cur_section):
            continue
        symbol_match = SYMBOL_RE.match(line.strip())
        # objdump symbol headers have no leading whitespace; be tolerant.
        if symbol_match is None:
            symbol_match = SYMBOL_RE.match(line)
        if symbol_match:
            cur_symbol = symbol_match.group(2)
            cur_keep = bool(hotspot.search(cur_symbol))
            if cur_keep:
                kept.append(raw)
                stats.setdefault(
                    cur_symbol,
                    {"instructions": 0, "vfmadd": 0, "call": 0, "spills": 0},
                )
            continue
        if cur_symbol is None or not cur_keep:
            continue
        if not INSTR_RE.match(line):
            continue
        kept.append(raw)
        entry = stats[cur_symbol]
        entry["instructions"] += 1
        if VFMADD_RE.search(line):
            entry["vfmadd"] += 1
        if CALL_RE.search(line):
            entry["call"] += 1
        if SPILL_RE.search(line):
            entry["spills"] += 1
        if ZMM_RE.search(line):
            zmm_hits.append(f"{cur_symbol}: {line.strip()}")
        if LIBM_RE.search(line):
            libm_hits.append(f"{cur_symbol}: {line.strip()}")
        if ALLOC_RE.search(line):
            alloc_hits.append(f"{cur_symbol}: {line.strip()}")

    header = (
        "# DSP hotspot report (filtered) — critical DSP loops only\n"
        f"# Source full dump: {args.full}\n"
        "# Sections: .text, .text.cold (excludes .bolt.org.text pre-BOLT duplicates)\n"
        f"# Symbols: {args.symbols}\n"
        "# Generator: utils/dsp-hotpath-filter.py (Sprint 3b/T3.4)\n"
    )
    try:
        with open(args.filtered, "w", encoding="utf-8") as handle:
            handle.write(header)
            handle.writelines(kept)
    except OSError as exc:
        print(f"[FATAL] cannot write filtered report: {exc}", file=sys.stderr)
        return 1

    total_instr = sum(v["instructions"] for v in stats.values())
    total_vfmadd = sum(v["vfmadd"] for v in stats.values())
    total_call = sum(v["call"] for v in stats.values())
    try:
        with open(args.summary, "w", encoding="utf-8") as handle:
            handle.write("DSP hotspot summary (per symbol)\n")
            handle.write(
                f"symbols={len(stats)} instructions={total_instr} "
                f"vfmadd={total_vfmadd} call={total_call} "
                f"zmm={len(zmm_hits)} libm={len(libm_hits)} "
                f"alloc={len(alloc_hits)}\n"
            )
            handle.write("symbol | instructions | vfmadd | call | spills(%rsp)\n")
            for symbol in sorted(stats):
                entry = stats[symbol]
                handle.write(
                    f"{symbol} | {entry['instructions']} | {entry['vfmadd']} | "
                    f"{entry['call']} | {entry['spills']}\n"
                )
            handle.write(f"gate zmm: {'FAIL' if zmm_hits else 'PASS'} ({len(zmm_hits)})\n")
            for hit in zmm_hits[:20]:
                handle.write(f"  zmm: {hit}\n")
            handle.write(
                f"gate libm: {'FAIL' if libm_hits else 'PASS'} ({len(libm_hits)})\n"
            )
            for hit in libm_hits[:20]:
                handle.write(f"  libm: {hit}\n")
            handle.write(
                f"gate alloc: {'FAIL' if alloc_hits else 'PASS'} ({len(alloc_hits)})\n"
            )
            for hit in alloc_hits[:20]:
                handle.write(f"  alloc: {hit}\n")
    except OSError as exc:
        print(f"[FATAL] cannot write summary: {exc}", file=sys.stderr)
        return 1

    failures: list[str] = []
    if zmm_hits:
        failures.append(f"static gate FAIL: {len(zmm_hits)} zmm use(s) (expected 0)")
    if libm_hits:
        failures.append(
            f"static gate FAIL: {len(libm_hits)} libm call(s) on the RT path "
            "(expected 0; current tanhf tail proves the gate bites until Sprint 4)"
        )
    if alloc_hits:
        failures.append(
            f"static gate FAIL: {len(alloc_hits)} alloc call(s) (expected 0)"
        )
    if not stats:
        failures.append("static gate FAIL: zero hotspot symbols matched the filter")
    if failures:
        for failure in failures:
            print(f"[FATAL] {failure}", file=sys.stderr)
        for hit in (zmm_hits + libm_hits + alloc_hits)[:10]:
            print(f"  hit: {hit}", file=sys.stderr)
        return 1

    print(
        f"  OK hotspot gates PASS: {len(stats)} symbols, {total_instr} instructions, "
        f"vfmadd={total_vfmadd}, zmm=0, libm=0, alloc=0"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
