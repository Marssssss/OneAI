# SWE-bench Lite — Phase 1 (manual path validation)

Goal of phase 1: prove end-to-end that **OneAI can produce a patch that
SWE-bench's evaluation harness accepts and judges**. We do this manually on a
single instance — no Rust changes, no batch runner. Just the TUI, `git`, and
two glue scripts.

> SWE-bench (Lite/Verified/full) is **only an evaluator**, not an agent. It
> consumes a JSONL of `{instance_id, model_name_or_path, model_patch}` and, per
> instance, applies the patch to a Docker image of the repo at `base_commit`,
> runs the test suite, and reports whether the issue's `FAIL_TO_PASS` tests now
> pass without regressing `PASS_TO_PASS`. OneAI's job is step ① (produce the
> patch); SWE-bench does step ② (judge it):
>
> ```
> ① OneAI (TUI)                 ② SWE-bench harness
> repo@base_commit ──edit──► patch ──► apply + run tests ──► pass/fail
> ```

## Prerequisites

- **Python 3.8+** with the SWE-bench harness installed:
  ```bash
  pip install swebench
  ```
- **Docker** (for step ②). SWE-bench needs `x86_64`, ≥120 GB disk, ≥16 GB RAM,
  ≥8 CPU cores. **Apple Silicon is experimental** — add `--namespace ''` to
  build images locally instead of pulling Linux images (slower).
- **git** on PATH, and network access to github.com + huggingface.co.

## Scripts

| Script | What it does |
|---|---|
| `export_dataset.py` | Export a SWE-bench split (Lite/Verified) to a local JSONL — the `--dataset` file `oneai eval swebench` consumes. Requires `pip install datasets`. |
| `fetch_instance.py` | Pull a Lite instance's `repo` / `base_commit` / `problem_statement` from HuggingFace. Prints the `git clone`+`checkout` commands and the issue text to paste into the TUI. |
| `make_prediction.py` | Turn the agent's `git diff` (or a saved patch file) into the SWE-bench JSONL. |

`export_dataset.py` is the phase-2 bridge: it materializes the dataset locally
so the Rust runner does no dataset-server networking. `fetch_instance.py` /
`make_prediction.py` remain the phase-1 manual single-instance tools.

Both are standalone Python — phase 1 deliberately stays outside the Rust
workspace. Phase 2 will fold instance loading + prediction export into
`oneai-eval` proper (`EvalCase::dataset` + an `ExternalJudge` / SWE-bench
serializer).

## End-to-end on one instance

The canonical smoke-test instance is `astropy__astropy-12907` (a clean bug in
`astropy.modeling.separable` with reproducible code in the issue).

### 1. Get the instance into the TUI

```bash
# From the repo root:
python3 scripts/swebench/fetch_instance.py astropy__astropy-12907
```

This prints the `git clone` + `git checkout <base_commit>` commands and the
full `problem_statement`. Run the clone/checkout in a **working directory you
point the TUI at** (do not edit inside the OneAI repo itself — keep it clean).

### 2. Drive OneAI in the TUI

Launch the TUI with its working directory set to the cloned repo's parent
(or use the `shell` tool's `working_dir`). Then prompt the agent, pasting the
`problem_statement`:

> "Here is a bug report for this codebase (checked out at the relevant
> commit). Reproduce it, locate the root cause, and fix it by editing the
> source. Do not commit — just leave the changes in the working tree.
>
> <paste problem_statement>"

The agent uses `read_file` / `grep` / `glob` / `edit_file` (or `shell`) to
locate and fix the bug. Watch it work — this is the point of doing it
manually: you see *how* OneAI navigates a real multi-file repo.

### 3. Capture the patch as a SWE-bench prediction

From the cloned repo (where the agent left uncommitted edits):

```bash
python3 scripts/swebench/make_prediction.py \
    --instance-id astropy__astropy-12907 \
    --repo ./path/to/astropy \
    --model oneai \
    --out predictions.jsonl
```

This runs `git diff`, wraps it as
`{"instance_id": ..., "model_name_or_path": "oneai", "model_patch": ...}`,
and writes one JSONL line. Verify the patch is non-empty first — an empty
patch means the agent didn't edit anything.

### 4. Judge it with SWE-bench

```bash
python3 -m swebench.harness.run_evaluation \
    --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path predictions.jsonl \
    --max_workers 1 \
    --run_id oneai-phase1-12907 \
    --namespace ''          # REQUIRED on Apple Silicon; remove on x86_64
```

`--max_workers 1` because we submitted a single instance. Results land in
`evaluation_results/<run_id>/`: `results.json` has the resolution rate,
`instance_results.jsonl` has the per-instance verdict (`resolved` true/false +
which tests failed). A `resolved: true` means the **full path is proven**.

## Tips & gotchas

- **Don't commit the fix.** The agent should leave changes unstaged so plain
  `git diff` captures them. If it committed, use `--scope head`
  (`git diff HEAD~`) instead.
- **The patch must apply to `base_commit`.** Working on a checkout of exactly
  that commit guarantees it. Don't `git pull` or merge afterward.
- **instance_id must match the dataset.** It's `owner__repo-issue_number`
  (double underscore). `fetch_instance.py` prints it; copy it verbatim into
  `--instance-id`.
- **Lite ≠ Verified instances.** SWE-bench Lite is curated for lower run cost,
  Verified for human-confirmed solvability — different subsets. A Lite score is
  not directly comparable to the Verified leaderboard; it's a relative signal.
  To publish on the leaderboard later, re-run on Verified.
- **arm64 slowness.** On Apple Silicon, `--namespace ''` builds each instance
  image locally (minutes per instance). For a single smoke test that's fine.
  At scale, run on an `x86_64` box or use [Modal](https://modal.com/)/[sb-cli](https://github.com/swe-bench/sb-cli).
