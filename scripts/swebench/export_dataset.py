#!/usr/bin/env python3
"""Export a SWE-bench dataset split to a local JSONL for the Rust runner.

`oneai eval swebench --dataset <path>` reads a local JSONL where each line is
one SWE-bench instance row (instance_id / repo / base_commit /
problem_statement / FAIL_TO_PASS / PASS_TO_PASS / version). This script
produces that file from the HuggingFace `datasets` library — the Rust side
deliberately does no dataset-server networking, so this is the bridge that
materializes the dataset locally.

The Rust `SwebenchInstance` loader accepts FAIL_TO_PASS / PASS_TO_PASS in
either form (string or JSON array), so we write whatever `datasets` gives us
verbatim — no normalization needed.

Usage:

    # Default: export the full SWE-bench_Lite test split (300 rows).
    python3 export_dataset.py
    python3 export_dataset.py --out swe_bench_lite.jsonl

    # Verified split (500 rows).
    python3 export_dataset.py --dataset princeton-nlp/SWE-bench_Verified \\
        --out swe_bench_verified.jsonl

    # Export only specific instance ids (handy for a focused smoke run).
    python3 export_dataset.py --instances astropy__astropy-12907,django__django-11099

Requires:  python3 -m pip install datasets
"""

import argparse
import json
import sys


def load_rows(dataset: str, split: str) -> list:
    try:
        from datasets import load_dataset
    except ImportError:
        sys.exit(
            "error: the `datasets` package is required. Install it with:\n"
            "  python3 -m pip install datasets"
        )
    try:
        ds = load_dataset(dataset, split=split)
    except Exception as e:  # network / repo-not-found / bad split, etc.
        sys.exit(f"error: failed to load {dataset} ({split}): {e}")
    return [dict(r) for r in ds]


def main() -> None:
    ap = argparse.ArgumentParser(description="Export a SWE-bench split to local JSONL.")
    ap.add_argument(
        "--dataset",
        default="princeton-nlp/SWE-bench_Lite",
        help="HuggingFace dataset id (default: princeton-nlp/SWE-bench_Lite)",
    )
    ap.add_argument("--split", default="test", help="dataset split (default: test)")
    ap.add_argument(
        "--out",
        default="swe_bench_lite.jsonl",
        help="output JSONL path (default: swe_bench_lite.jsonl)",
    )
    ap.add_argument(
        "--instances",
        help="comma-separated instance ids to keep (default: all rows)",
    )
    args = ap.parse_args()

    rows = load_rows(args.dataset, args.split)

    if args.instances:
        wanted = {s.strip() for s in args.instances.split(",") if s.strip()}
        before = len(rows)
        rows = [r for r in rows if r.get("instance_id") in wanted]
        missing = wanted - {r.get("instance_id", "") for r in rows}
        if missing:
            print(
                f"warning: {len(missing)} requested id(s) not found in "
                f"{args.dataset} ({args.split}): {', '.join(sorted(missing))}",
                file=sys.stderr,
            )
        print(f"filtered {before} → {len(rows)} rows by --instances", file=sys.stderr)

    try:
        with open(args.out, "w", encoding="utf-8") as f:
            for row in rows:
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
    except OSError as e:
        sys.exit(f"error: cannot write to {args.out}: {e}")

    print(
        f"wrote {len(rows)} instance(s) to {args.out} "
        f"(from {args.dataset} {args.split})",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
