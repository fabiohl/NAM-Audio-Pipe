#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
"""Unified RT-codegen disassembly filter and static gate (NAM ecosystem).

Reads a full objdump -d --demangle dump, keeps only .text / .text.cold sections,
filters symbols by profile, writes reports and enforces fail-closed static gates.
"""

from __future__ import annotations

import argparse
import json
import re
import sys


SECTION_RE = re.compile(r"^\s*Disassembly of section (\S+?):\s*$")
SYMBOL_RE = re.compile(r"^([0-9a-fA-F]+)\s+<(.+)>:\s*$")
INSTR_RE = re.compile(r"^\s+[0-9a-fA-F]+:\s+")
VFMADD_RE = re.compile(r"vfmadd", re.IGNORECASE)
CALL_RE = re.compile(r"\bcallq?\b")
SPILL_RE = re.compile(r"\(%rsp\)")
# AVX-512 / EVEX: zmm registers, xmm16-31, ymm16-31, opmasks %k0-7, {z}, {1toN}
ZMM_RE = re.compile(
    r"\b(zmm\d+|[xy]mm(1[6-9]|2[0-9]|3[01])|%?k[0-7])\b|\{(%?k[0-7]|z|1to\d+)\}",
    re.IGNORECASE,
)
LIBM_RE = re.compile(
    r"call\w*\s+.*<(tanhf?|expf?|logf?|powf?|sinf?|cosf?|atan2?f?|"
    r"atanf?|sqrtf?|fmodf?|sincosf?)(@|>)"
)
ALLOC_RE = re.compile(
    r"\bcall\w*\s+.*(__rust_alloc|__rust_dealloc|__rust_realloc|__rg_alloc|"
    r"<alloc::|\bmalloc\b|\bcalloc\b|\brealloc\b|\bfree\b)"
)
DIV_RE = re.compile(r"\b(i?div[bwdlq]?)\b")
TLS_RE = re.compile(r"\bcall\w*\s+.*__tls_get_addr")

DEFAULT_SECTIONS = (".text", ".text.cold")
GATE_KEYS = ("zmm", "libm", "alloc", "div", "tls")
SYNTHETIC_VIOLATION = "SYNTHETIC: violation injected by --test-synthetic-violation"


def is_kept_section(section: str, sections: tuple[str, ...]) -> bool:
    """Accept .text and .text.cold* (reject pre-BOLT duplicates)."""
    return any(section == s or section.startswith(s + ".") for s in sections)


def parse_args() -> argparse.Namespace:
    """Parse CLI options for the disassembly filter."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", required=True, help="Full objdump dump input")
    parser.add_argument("--filtered", required=True, help="Filtered hotspot output")
    parser.add_argument("--summary", required=True, help="Per-symbol summary output")
    parser.add_argument("--symbols", help="Regex selecting hotspot symbols")
    parser.add_argument("--profile", help="JSON profile selecting targets and gates")
    parser.add_argument(
        "--test-synthetic-violation",
        action="store_true",
        help="Inject synthetic violation to prove gate fails closed",
    )
    args = parser.parse_args()
    if not args.symbols and not args.profile:
        parser.error("one of --symbols or --profile is required")
    return args


def load_profile(args: argparse.Namespace) -> tuple[list[dict], tuple[str, ...]]:
    """Resolve profile into target specs and kept sections."""
    sections = DEFAULT_SECTIONS
    targets: list[dict] = []
    if args.profile:
        try:
            with open(args.profile, "r", encoding="utf-8") as h:
                doc = json.load(h)
        except (OSError, json.JSONDecodeError) as exc:
            print(f"[FATAL] cannot read profile {args.profile}: {exc}", file=sys.stderr)
            raise SystemExit(1)
        if not isinstance(doc, dict):
            print(f"[FATAL] profile must be an object: {args.profile}", file=sys.stderr)
            raise SystemExit(1)
        if isinstance(doc.get("sections"), list) and doc["sections"]:
            sections = tuple(str(s) for s in doc["sections"])
        if isinstance(doc.get("targets"), list) and doc["targets"]:
            for raw in doc["targets"]:
                if not isinstance(raw, dict) or "pattern" not in raw:
                    print(f"[FATAL] malformed target in {args.profile}", file=sys.stderr)
                    raise SystemExit(1)
                targets.append({
                    "id": str(raw.get("id") or raw["pattern"]),
                    "pattern": str(raw["pattern"]),
                    "required": bool(raw.get("required", True)),
                    "gates": dict(raw.get("gates") or {}),
                    "expected_methods": list(raw.get("expected_methods") or []),
                    "exceptions": {k: list(v) for k, v in (raw.get("exceptions") or {}).items()},
                    "simple": False,
                })
        elif doc.get("symbols"):
            targets.append({
                "id": str(doc.get("id") or "hotspot"),
                "pattern": str(doc["symbols"]),
                "required": True,
                "gates": dict(doc.get("gates") or {}),
                "expected_methods": [],
                "exceptions": {},
                "simple": True,
            })
    if args.symbols:
        targets.append({
            "id": "hotspot",
            "pattern": args.symbols,
            "required": True,
            "gates": {"zmm": 0, "libm": 0, "alloc": 0},
            "expected_methods": [],
            "exceptions": {},
            "simple": True,
        })
    for target in targets:
        gates = target["gates"]
        if target["simple"]:
            for key in ("zmm", "libm", "alloc"):
                gates.setdefault(key, 0)
        for key in GATE_KEYS:
            val = gates.get(key)
            if val is not None:
                try:
                    gates[key] = int(val)
                except (TypeError, ValueError):
                    print(f"[FATAL] gate {key} must be int/null", file=sys.stderr)
                    raise SystemExit(1)
        try:
            target["_regex"] = re.compile(target["pattern"])
        except re.error as exc:
            print(f"[FATAL] invalid regex for {target['id']}: {exc}", file=sys.stderr)
            raise SystemExit(1)
    return targets, sections


def main() -> int:
    """Filter, summarize and enforce gates on the dump; return exit code."""
    args = parse_args()
    targets, sections = load_profile(args)
    try:
        with open(args.full, "r", encoding="utf-8", errors="replace") as h:
            lines = h.readlines()
    except OSError as exc:
        print(f"[FATAL] cannot read {args.full}: {exc}", file=sys.stderr)
        return 1

    kept: list[str] = []
    matched: dict[tuple[str, str], dict] = {}
    failures: list[str] = []
    cur_section = ""
    cur_symbol: str | None = None
    cur_targets: list[dict] = []

    for raw in lines:
        line = raw.rstrip("\n")
        sm = SECTION_RE.match(line)
        if sm:
            cur_section = sm.group(1)
            cur_symbol = None
            cur_targets = []
            if is_kept_section(cur_section, sections):
                kept.append(raw)
            continue
        if not is_kept_section(cur_section, sections):
            continue
        sym_m = SYMBOL_RE.match(line.strip()) or SYMBOL_RE.match(line)
        if sym_m:
            cur_symbol = sym_m.group(2)
            cur_targets = [t for t in targets if t["_regex"].search(cur_symbol)]
            for t in cur_targets:
                if t["expected_methods"] and not any(m in cur_symbol for m in t["expected_methods"]):
                    failures.append(f"target '{t['id']}': unexpected method '{cur_symbol}'")
                key = (cur_symbol, t["id"])
                matched.setdefault(key, {
                    "symbol": cur_symbol, "target": t,
                    "stats": {"instructions": 0, "vfmadd": 0, "call": 0, "spills": 0},
                    "hits": {k: [] for k in GATE_KEYS},
                    "exceptions": {k: [] for k in GATE_KEYS},
                })
            if cur_targets:
                kept.append(raw)
            continue
        if not cur_symbol or not cur_targets:
            continue
        if not INSTR_RE.match(line):
            continue
        kept.append(raw)
        for t in cur_targets:
            entry = matched[(cur_symbol, t["id"])]
            entry["stats"]["instructions"] += 1
            if VFMADD_RE.search(line):
                entry["stats"]["vfmadd"] += 1
            if CALL_RE.search(line):
                entry["stats"]["call"] += 1
            if SPILL_RE.search(line):
                entry["stats"]["spills"] += 1
            for k, regex in (("zmm", ZMM_RE), ("libm", LIBM_RE), ("alloc", ALLOC_RE),
                             ("div", DIV_RE), ("tls", TLS_RE)):
                if regex.search(line):
                    excs = t["exceptions"].get(k, [])
                    if any(e in line for e in excs):
                        entry["exceptions"][k].append(line.strip())
                    else:
                        entry["hits"][k].append(line.strip())

    header = (
        "# DSP hotspot report (filtered) — critical DSP loops only\n"
        f"# Source full dump: {args.full}\n"
        f"# Sections: {', '.join(sections)} (pre-BOLT duplicates excluded)\n"
        f"# Targets: {', '.join(t['id'] for t in targets)}\n"
        "# Generator: utils/dsp-hotpath-filter.py (unified NAM codegen filter)\n"
    )
    try:
        with open(args.filtered, "w", encoding="utf-8") as h:
            h.write(header)
            h.writelines(kept)
    except OSError as exc:
        print(f"[FATAL] cannot write filtered report: {exc}", file=sys.stderr)
        return 1

    total_instr = sum(v["stats"]["instructions"] for v in matched.values())
    total_vfmadd = sum(v["stats"]["vfmadd"] for v in matched.values())
    total_call = sum(v["stats"]["call"] for v in matched.values())

    target_symbol_count = {t["id"]: 0 for t in targets}
    for entry in matched.values():
        target_symbol_count[entry["target"]["id"]] += 1

    gate_lines: list[str] = []
    for target in targets:
        tid = target["id"]
        c = target_symbol_count[tid]
        if c == 0:
            if target["required"]:
                failures.append(f"target '{tid}': zero matching symbols (required, fail-closed)")
                gate_lines.append(f"target {tid}: MISSING (required)")
            else:
                gate_lines.append(f"target {tid}: ABSENT (optional/inlined)")
            continue
        gate_lines.append(f"target {tid}: PRESENT ({c} symbol(s))")
        for key in GATE_KEYS:
            budget = target["gates"].get(key)
            if budget is None:
                continue
            hits = [l for e in matched.values() if e["target"] is target for l in e["hits"][key]]
            excs = sum(len(e["exceptions"][key]) for e in matched.values() if e["target"] is target)
            status = "PASS" if len(hits) <= budget else "FAIL"
            note = f", {excs} exception(s) permitted" if excs else ""
            gate_lines.append(f"target {tid} gate {key}: {status} ({len(hits)} <= {budget}{note})")
            if len(hits) > budget:
                failures.append(f"target '{tid}': {len(hits)} {key} hit(s) > budget {budget}")

    if args.test_synthetic_violation:
        failures.append(SYNTHETIC_VIOLATION)

    try:
        with open(args.summary, "w", encoding="utf-8") as h:
            h.write("DSP hotspot summary (per symbol)\n")
            h.write(
                f"symbols={len(matched)} instructions={total_instr} "
                f"vfmadd={total_vfmadd} call={total_call}\n"
            )
            h.write("symbol | target | instructions | vfmadd | call | spills(%rsp)\n")
            for key in sorted(matched):
                e = matched[key]
                h.write(
                    f"{e['symbol']} | {e['target']['id']} | {e['stats']['instructions']} | "
                    f"{e['stats']['vfmadd']} | {e['stats']['call']} | {e['stats']['spills']}\n"
                )
            for gl in gate_lines:
                h.write(gl + "\n")
            for f in failures:
                h.write(f"FAIL: {f}\n")
    except OSError as exc:
        print(f"[FATAL] cannot write summary: {exc}", file=sys.stderr)
        return 1

    if failures:
        print(f"[FATAL] RT codegen gate FAILED ({args.full}):", file=sys.stderr)
        for f in failures:
            print(f"  ❌ {f}", file=sys.stderr)
            if f == SYNTHETIC_VIOLATION:
                continue
        shown = 0
        for entry in matched.values():
            for key in GATE_KEYS:
                if entry["target"]["gates"].get(key) is None:
                    continue
                for l in entry["hits"][key][:1]:
                    print(f"  hit[{key}]: {l}", file=sys.stderr)
                    shown += 1
                    if shown >= 10:
                        break
            if shown >= 10:
                break
        return 1

    print(
        f"  OK hotspot gates PASS: {len(matched)} symbols, {total_instr} instructions, "
        f"vfmadd={total_vfmadd}, targets={len(targets)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
