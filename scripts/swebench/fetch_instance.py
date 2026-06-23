#!/usr/bin/env python3
"""Fetch a SWE-bench instance's metadata + problem statement.

Phase-1 glue: this is the bridge that gives you exactly what to paste into the
OneAI TUI when manually driving the agent on a single SWE-bench Lite instance.
It pulls the instance row from HuggingFace (no auth needed) and prints:

  1. The `repo` + `base_commit` to checkout.
  2. A ready-to-run `git clone` + `git checkout` command.
  3. The full `problem_statement` (the issue text → paste into TUI).

Usage:
    python3 fetch_instance.py <instance_id>
    python3 fetch_instance.py astropy__astropy-12907
    python3 fetch_instance.py --list            # list the first N instance ids
    python3 fetch_instance.py --list 20

The HuggingFace datasets-server API returns JSON rows; we filter client-side
by `instance_id`. For `--list` we only print ids (cheap). For a specific id we
do a linear scan of the split in pages until found — Lite is only 300 rows so
this completes in a handful of requests.

NOTE: This is intentionally a standalone Python script, not part of oneai-eval.
Phase 1 keeps everything outside the Rust workspace; phase 2 will fold instance
loading + SWE-bench prediction export into oneai-eval proper.
"""

import argparse
import json
import sys
import time
import urllib.parse
import urllib.request
import urllib.error
from http.client import IncompleteRead

DATASET = "princeton-nlp/SWE-bench_Lite"
SPLIT = "test"
PAGE_SIZE = 25  # small pages → small responses → fewer mid-read timeouts
HF_ROWS_URL = (
    "https://datasets-server.huggingface.co/rows"
    "?dataset={dataset}&config=default&split={split}&offset={offset}&length={length}"
)


def _fetch_page(offset: int, length: int) -> list:
    url = HF_ROWS_URL.format(
        dataset=urllib.parse.quote(DATASET, safe=""),
        split=SPLIT,
        offset=offset,
        length=length,
    )
    req = urllib.request.Request(url, headers={"User-Agent": "oneai-swebench-phase1/1.0"})
    # HF datasets-server is flaky: it times out mid-read on larger pages and
    # sometimes drops the connection. Catch the whole OSError family (covers
    # URLError, TimeoutError, ConnectionError) plus IncompleteRead, then retry
    # with backoff. A fresh Request object per attempt avoids stale connection.
    last_err = None
    for attempt in range(5):
        try:
            this_req = urllib.request.Request(url, headers={"User-Agent": "oneai-swebench-phase1/1.0"})
            with urllib.request.urlopen(this_req, timeout=90) as resp:
                body = resp.read()
            payload = json.loads(body)
            return [r["row"] for r in payload.get("rows", [])]
        except (OSError, IncompleteRead) as e:
            last_err = e
            time.sleep(1.0 * (attempt + 1))  # 1s, 2s, 3s, 4s, 5s backoff
            continue
    sys.exit(f"error: failed to reach HuggingFace datasets-server after 5 retries: {last_err}")


def list_instances(limit: int) -> None:
    seen = 0
    offset = 0
    while seen < limit:
        rows = _fetch_page(offset, PAGE_SIZE)
        if not rows:
            break
        for row in rows:
            print(row.get("instance_id", "?"))
            seen += 1
            if seen >= limit:
                return
        offset += PAGE_SIZE


def find_instance(instance_id: str) -> dict | None:
    offset = 0
    while True:
        rows = _fetch_page(offset, PAGE_SIZE)
        if not rows:
            return None
        for row in rows:
            if row.get("instance_id") == instance_id:
                return row
        offset += PAGE_SIZE


def main() -> None:
    ap = argparse.ArgumentParser(description="Fetch a SWE-bench Lite instance.")
    ap.add_argument("instance_id", nargs="?", help="e.g. astropy__astropy-12907")
    ap.add_argument(
        "--list",
        nargs="?",
        const=10,
        type=int,
        metavar="N",
        help="list the first N instance ids (default 10)",
    )
    args = ap.parse_args()

    if args.list is not None:
        list_instances(args.list)
        return

    if not args.instance_id:
        ap.error("instance_id is required (or use --list)")

    row = find_instance(args.instance_id)
    if row is None:
        sys.exit(f"error: instance '{args.instance_id}' not found in {DATASET} ({SPLIT})")

    repo = row.get("repo", "")
    base_commit = row.get("base_commit", "")
    problem = row.get("problem_statement", "")
    version = row.get("version", "")

    print(f"instance_id : {args.instance_id}")
    print(f"repo        : {repo}")
    print(f"base_commit : {base_commit}")
    print(f"version     : {version}")
    print()
    print("# ── clone + checkout (run this in your TUI working dir) ─────────────")
    print(f"git clone https://github.com/{repo}.git")
    safe_repo = repo.split("/")[-1]
    print(f"cd {safe_repo}")
    print(f"git checkout {base_commit}")
    print()
    print("# ── problem_statement (paste into TUI as the issue) ───────────────")
    print("------------------------------------------------------------------------")
    print(problem)
    print("------------------------------------------------------------------------")


if __name__ == "__main__":
    main()
