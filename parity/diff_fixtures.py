#!/usr/bin/env python
"""Compare two fixture trees (e.g. two dumps with different PYTHONHASHSEED) and list the JSON
paths that differ - this is how the non-deterministic outputs in ARCHITECTURE.md were found.

  python parity/diff_fixtures.py DIR_A DIR_B [--ignore now,generated_at,...] [--max 20]
"""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
from typing import Any

DEFAULT_IGNORE = {"now", "generated_at", "worktree_head", "archived_at", "created", "fetched_at", "age_seconds"}


def diff(a: Any, b: Any, path: str, out: list[str], ignore: set[str], rel: float) -> None:
    if isinstance(a, dict) and isinstance(b, dict):
        for k in list(a.keys()) + [k for k in b.keys() if k not in a]:
            if k in ignore:
                continue
            if k not in a or k not in b:
                out.append(f"{path}.{k}: missing on {'a' if k not in a else 'b'}")
                continue
            diff(a[k], b[k], f"{path}.{k}", out, ignore, rel)
        if list(a.keys()) != list(b.keys()) and set(a.keys()) == set(b.keys()):
            out.append(f"{path}: key ORDER differs")
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            out.append(f"{path}: len {len(a)} != {len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", out, ignore, rel)
    elif isinstance(a, float) or isinstance(b, float):
        if not (isinstance(a, (int, float)) and isinstance(b, (int, float))) or \
                not math.isclose(a, b, rel_tol=rel, abs_tol=rel):
            out.append(f"{path}: {a!r} != {b!r}")
    elif a != b:
        out.append(f"{path}: {str(a)[:100]!r} != {str(b)[:100]!r}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--ignore", default="")
    ap.add_argument("--max", type=int, default=15)
    ap.add_argument("--rel", type=float, default=0.0)
    args = ap.parse_args()
    ignore = DEFAULT_IGNORE | {x for x in args.ignore.split(",") if x}
    ra, rb = Path(args.a), Path(args.b)
    for fa in sorted(ra.rglob("*.json")):
        fb = rb / fa.relative_to(ra)
        if not fb.exists():
            print(f"{fa.relative_to(ra)}: missing in b")
            continue
        out: list[str] = []
        diff(json.loads(fa.read_text(encoding="utf-8")), json.loads(fb.read_text(encoding="utf-8")), "$", out,
             ignore, args.rel)
        if out:
            print(f"== {fa.relative_to(ra)}: {len(out)} differences")
            for line in out[: args.max]:
                print("   ", line)


if __name__ == "__main__":
    main()
