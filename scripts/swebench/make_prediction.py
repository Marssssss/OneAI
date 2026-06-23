#!/usr/bin/env python3
"""Assemble a SWE-bench prediction file (JSONL) from an agent's patch.

SWE-bench's evaluation harness does not care *how* you produced a patch — it
only consumes a JSONL file where each line is:

    {"instance_id": "owner__repo-issue", "model_name_or_path": "x", "model_patch": "<unified diff>"}

After driving OneAI in the TUI on a checked-out repo, the patch is simply
`git diff` of that working tree. This script turns that diff (from a file, or
captured directly from a repo via `git diff`) into the JSONL above.

Usage:

    # Capture the patch from the repo OneAI just edited, one instance:
    python3 make_prediction.py \\
        --instance-id astropy__astropy-12907 \\
        --repo ./work/astropy \\
        --model oneai \\
        --out predictions.jsonl

    # Or supply an already-saved diff file:
    python3 make_prediction.py \\
        --instance-id astropy__astropy-12907 \\
        --patch-file my.patch \\
        --out predictions.jsonl

    # Append more instances into the same file (--append):
    python3 make_prediction.py --instance-id ... --repo ... --out predictions.jsonl --append

The `model_patch` must be a unified diff that applies cleanly to the instance's
`base_commit`. Using `git diff` on a checkout of that commit guarantees this.
Staged vs unstaged: by default we run plain `git diff` (unstaged changes). Pass
`--staged` to use `git diff --cached`. Pass `--head` to diff against the first
parent commit (`git diff HEAD~`) if the agent committed the fix.

NOTE: standalone Python glue for phase 1. Phase 2 will move prediction export
into oneai-eval as a proper EvalReport serializer.
"""

import argparse
import json
import subprocess
import sys


def patch_from_repo(repo: str, scope: str) -> str:
    """Run `git diff` in `repo` and return the patch text."""
    args = ["git", "-C", repo, "diff"]
    if scope == "staged":
        args.append("--cached")
    elif scope == "head":
        args = ["git", "-C", repo, "diff", "HEAD~"]
    # "unstaged" (default) → plain `git diff`
    try:
        result = subprocess.run(args, capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as e:
        sys.exit(f"error: git diff failed in {repo}: {e.stderr.strip() or e}")
    except FileNotFoundError:
        sys.exit("error: git not found on PATH")
    return result.stdout


def patch_from_file(path: str) -> str:
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except OSError as e:
        sys.exit(f"error: cannot read patch file {path}: {e}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Assemble a SWE-bench prediction JSONL.")
    ap.add_argument("--instance-id", required=True, help="e.g. astropy__astropy-12907")
    ap.add_argument("--model", default="oneai", help="model_name_or_path (default: oneai)")
    ap.add_argument("--repo", help="path to a git repo; capture patch via `git diff`")
    ap.add_argument("--patch-file", help="path to a file containing a unified diff")
    ap.add_argument(
        "--scope",
        choices=["unstaged", "staged", "head"],
        default="unstaged",
        help="which changes to capture when using --repo (default: unstaged)",
    )
    ap.add_argument("--out", default="-", help="output JSONL path (default: stdout)")
    ap.add_argument("--append", action="store_true", help="append to --out instead of overwriting")
    args = ap.parse_args()

    if not args.repo and not args.patch_file:
        ap.error("provide either --repo or --patch-file")
    if args.repo and args.patch_file:
        ap.error("--repo and --patch-file are mutually exclusive")

    if args.repo:
        patch = patch_from_repo(args.repo, args.scope)
    else:
        patch = patch_from_file(args.patch_file)

    # An empty patch means the agent made no changes — almost certainly a mistake
    # for a SWE-bench task. Warn loudly rather than silently writing a no-op line.
    if not patch.strip():
        sys.exit(
            "error: patch is empty. Did the agent actually edit files? "
            "(check `git status` in the repo, or use a different --scope)"
        )

    record = {
        "instance_id": args.instance_id,
        "model_name_or_path": args.model,
        "model_patch": patch,
    }
    line = json.dumps(record, ensure_ascii=False)

    if args.out == "-":
        print(line)
        return

    mode = "a" if args.append else "w"
    try:
        with open(args.out, mode, encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError as e:
        sys.exit(f"error: cannot write to {args.out}: {e}")

    verb = "appended to" if args.append else "wrote"
    print(f"{verb} {args.out}: {args.instance_id} ({len(patch)} patch bytes)", file=sys.stderr)


if __name__ == "__main__":
    main()
