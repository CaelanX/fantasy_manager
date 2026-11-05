#!/usr/bin/env python
"""Print the shape of a fixture JSON file (keys, types, a sample value per path) - a quick way
for translators to see what a fixture holds without loading megabytes into an editor.

  python rust/parity/shapes.py rust/parity/fixtures/espn/snapshot.json [max_depth] [path.prefix]
"""
from __future__ import annotations

import json
import sys
from typing import Any


def walk(x: Any, path: str, depth: int, max_depth: int, seen: set[str], prefix: str) -> None:
    key = path if path else "$"
    if isinstance(x, dict):
        t = f"object[{len(x)}]"
    elif isinstance(x, list):
        t = f"array[{len(x)}]"
    else:
        t = type(x).__name__
    norm = key
    if norm not in seen and norm.startswith(prefix[: len(norm)] if len(norm) < len(prefix) else prefix):
        seen.add(norm)
        sample = "" if isinstance(x, (dict, list)) else " = " + json.dumps(x, ensure_ascii=False)[:80]
        if norm.startswith(prefix):
            print(f"{'  ' * depth}{norm}: {t}{sample}")
    if depth >= max_depth:
        return
    if isinstance(x, dict):
        keys = list(x.keys())
        # map-like objects (ids / dates as keys): show the first entry only
        maplike = len(keys) > 60 or (bool(keys) and all(k[:1].isdigit() for k in keys))
        for k in (keys[:1] if maplike and keys else keys):
            walk(x[k], f"{path}.{'<k>' if maplike else k}", depth + 1, max_depth, seen, prefix)
    elif isinstance(x, list) and x:
        walk(x[0], f"{path}[]", depth + 1, max_depth, seen, prefix)


def main() -> None:
    data = json.load(open(sys.argv[1], encoding="utf-8"))
    max_depth = int(sys.argv[2]) if len(sys.argv) > 2 else 6
    prefix = sys.argv[3] if len(sys.argv) > 3 else ""
    walk(data, "", 0, max_depth, set(), prefix)


if __name__ == "__main__":
    main()
