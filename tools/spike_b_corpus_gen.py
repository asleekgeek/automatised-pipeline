#!/usr/bin/env python3
"""Generate per-file expected-counts JSON for the Spike B' full-corpus gate.

For every .py file under <target_dir>, computes the structural counts the
Rust extractor must produce: import count, constant count (UPPER_SNAKE),
function count (top-level), class count (Struct nodes), method count
(class members), and call-site count (every ast.Call inside any function
or method body). Also captures the file's relative path so the test
harness can mirror it under the indexed tempdir for QN consistency.

Output format (JSON):
{
  "target_root": "/abs/path/to/scanned/root",
  "files": [
    {
      "rel_path": "shared/text.py",
      "imports":   2,
      "constants": 3,
      "functions": 2,
      "classes":   0,
      "methods":   0,
      "callsites": 7,
      "intra_pairs": 1
    },
    ...
  ]
}

The Rust test loads this, indexes each file, and asserts the extractor
emits the EXACT 5-tuple (imports, constants, functions, classes, methods)
that Python's `ast` produces. We measured this passes on Cortex's 576-file
tree as of 2026-05-25; the test fails on any drift.

`callsites` and `intra_pairs` are emitted for future use but NOT asserted
because chained-call counting and resolver dedup diverge between Python
ast and the Rust resolver (e.g., `a.b().c()` is two ast.Call nodes but
typically one resolved Calls edge after dedup). When that divergence is
characterized and bounded these become assertable too.

source: Spike B' full-extension gate — 5-files-per-category was the
proof of harness; full coverage validates every file under the
indexed roots.
"""
from __future__ import annotations

import argparse
import ast
import json
import sys
from pathlib import Path


def is_upper_snake(name: str) -> bool:
    """Matches the Rust parser's UPPER_SNAKE_CASE constant detector."""
    if not name:
        return False
    if not all(c.isupper() or c == "_" or c.isdigit() for c in name):
        return False
    return any(c.isupper() for c in name)


def count_calls(body: list[ast.stmt]) -> int:
    """Count every ast.Call node inside the given statement list (recursive)."""
    n = 0
    for stmt in body:
        for child in ast.walk(stmt):
            if isinstance(child, ast.Call):
                n += 1
    return n


def analyze_file(path: Path) -> dict | None:
    """Return the structural counts dict for one .py file, or None on error."""
    try:
        src = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return None
    try:
        tree = ast.parse(src)
    except SyntaxError:
        return None

    imports = 0
    constants = 0
    functions = 0
    classes = 0
    methods = 0
    callsites = 0
    top_fn_names: set[str] = set()
    intra_pairs: set[tuple[str, str]] = set()

    for node in ast.iter_child_nodes(tree):
        if isinstance(node, ast.Import):
            imports += len(node.names)
        elif isinstance(node, ast.ImportFrom):
            imports += len(node.names)
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            if is_upper_snake(node.target.id):
                constants += 1
        elif isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name) and is_upper_snake(t.id):
                    constants += 1
        elif isinstance(node, ast.ClassDef):
            classes += 1
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    methods += 1
                    callsites += count_calls(item.body)
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            functions += 1
            top_fn_names.add(node.name)
            callsites += count_calls(node.body)

    # Intra-file Function → top-level-Function pairs (resolved Calls expected
    # to materialize as Calls_Function_Function edges after the resolver
    # pass; deduplicated per pair).
    for node in ast.iter_child_nodes(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for child in ast.walk(node):
                if isinstance(child, ast.Call) and isinstance(child.func, ast.Name):
                    if child.func.id in top_fn_names:
                        intra_pairs.add((node.name, child.func.id))

    return {
        "imports": imports,
        "constants": constants,
        "functions": functions,
        "classes": classes,
        "methods": methods,
        "callsites": callsites,
        "intra_pairs": len(intra_pairs),
    }


def walk(root: Path) -> list[dict]:
    """Walk every .py file under root, return one entry per file."""
    out: list[dict] = []
    for path in sorted(root.rglob("*.py")):
        # Skip cache / hidden / __init__-only churn — those carry no
        # structural signal and inflate fixture noise.
        rel_str = str(path.relative_to(root))
        if any(part.startswith(".") or part == "__pycache__" for part in path.parts):
            continue
        counts = analyze_file(path)
        if counts is None:
            continue
        # Skip empty / pure-docstring files — their counts are all zero
        # and they add no signal to the gate.
        if all(v == 0 for v in counts.values()):
            continue
        out.append({"rel_path": rel_str, **counts})
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("target_root", type=Path, help="root directory to scan")
    p.add_argument(
        "--out",
        type=Path,
        required=True,
        help="output JSON path",
    )
    args = p.parse_args()

    root = args.target_root.resolve()
    if not root.is_dir():
        print(f"error: {root} is not a directory", file=sys.stderr)
        return 1

    files = walk(root)
    payload = {"target_root": str(root), "files": files}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    totals = {
        k: sum(f[k] for f in files)
        for k in ("imports", "constants", "functions", "classes", "methods", "callsites", "intra_pairs")
    }
    print(
        f"wrote {len(files)} file entries to {args.out}\n"
        f"  totals: imports={totals['imports']:,} constants={totals['constants']:,} "
        f"functions={totals['functions']:,} classes={totals['classes']:,} "
        f"methods={totals['methods']:,} callsites={totals['callsites']:,} "
        f"intra_pairs={totals['intra_pairs']:,}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
