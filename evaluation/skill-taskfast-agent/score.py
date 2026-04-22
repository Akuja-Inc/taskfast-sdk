"""
Locked scorer for the taskfast-agent skill autoresearch loop.

Input:  traces/latest_eval.json  (written by run_eval.py)
Output: JSON {score, success_rate, cases, per_category_counts} to stdout
Side effects: appends one row to results.tsv

DO NOT EDIT weights, categories, or the hard-fail rules. This file is locked
by the optimizer program — see program.md.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
TRACE = HERE / "traces" / "latest_eval.json"
RESULTS = HERE / "results.tsv"

# Weighted scorer. Lower is better. See plan: i-need-to-improve-stateful-moth.md.
WEIGHTS = {
    "fatal": 1000,
    "claimed_success_but_failed": 1200,
    "required_final_state_not_ok": 500,
    "required_api_sequence_prefix_not_ok": 300,
    "wrong_order": 200,
    "schema_error": 150,
    "invalid_args": 150,
    "timeout": 80,
    "nonretryable_retried": 80,
    "missing_retry": 60,
    "excess_retry": 25,
    "recoverable": 20,
}

RESULTS_HEADER = [
    "run_id", "timestamp", "score", "success_rate", "cases",
    "golden_failures", "fatal", "claimed_success_but_failed",
    "wrong_order", "schema_error", "invalid_args",
    "missing_retry", "excess_retry", "nonretryable_retried",
    "timeout", "recoverable",
    "model_agent", "model_optimizer",
    "hypothesis",
]


def score_case(case: dict) -> int:
    counts = case.get("counts", {})
    s = 0
    for key, weight in WEIGHTS.items():
        s += weight * counts.get(key, 0)
    if not case.get("required_final_state_ok", True):
        s += WEIGHTS["required_final_state_not_ok"]
    if not case.get("required_api_sequence_prefix_ok", True):
        s += WEIGHTS["required_api_sequence_prefix_not_ok"]
    if case.get("claimed_success_but_failed", False):
        s += WEIGHTS["claimed_success_but_failed"]
    return s


def aggregate_counts(cases: list[dict]) -> dict[str, int]:
    agg: dict[str, int] = {k: 0 for k in WEIGHTS}
    for c in cases:
        for k in WEIGHTS:
            agg[k] += c.get("counts", {}).get(k, 0)
        if not c.get("required_final_state_ok", True):
            agg["required_final_state_not_ok"] += 1
        if not c.get("required_api_sequence_prefix_ok", True):
            agg["required_api_sequence_prefix_not_ok"] += 1
        if c.get("claimed_success_but_failed", False):
            agg["claimed_success_but_failed"] += 1
    return agg


def ensure_results_header() -> None:
    if not RESULTS.exists() or RESULTS.stat().st_size == 0:
        RESULTS.write_text("\t".join(RESULTS_HEADER) + "\n")


def append_results_row(row: dict) -> None:
    ensure_results_header()
    with RESULTS.open("a") as f:
        f.write("\t".join(str(row.get(col, "")) for col in RESULTS_HEADER) + "\n")


def main() -> int:
    if not TRACE.exists():
        print(f"ERROR: {TRACE} not found. Run run_eval.py first.", file=sys.stderr)
        return 2

    data = json.loads(TRACE.read_text())
    cases = data.get("cases", [])
    n = len(cases)
    if n == 0:
        print(json.dumps({"score": 0, "success_rate": 0.0, "cases": 0}), flush=True)
        return 1

    total_score = sum(score_case(c) for c in cases)
    passed = sum(1 for c in cases if c.get("passed", False))
    golden = [c for c in cases if c.get("category") == "golden"]
    golden_failures = sum(1 for c in golden if not c.get("passed", False))
    success_rate = passed / n
    per_category = aggregate_counts(cases)

    result = {
        "score": total_score,
        "success_rate": round(success_rate, 4),
        "cases": n,
        "golden_failures": golden_failures,
        "per_category_counts": per_category,
        "run_id": data.get("run_id", "unknown"),
    }
    print(json.dumps(result, indent=2), flush=True)

    append_results_row({
        "run_id": data.get("run_id", "unknown"),
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "score": total_score,
        "success_rate": round(success_rate, 4),
        "cases": n,
        "golden_failures": golden_failures,
        **{k: per_category[k] for k in WEIGHTS.keys() if k in per_category},
        "model_agent": data.get("model_agent", ""),
        "model_optimizer": data.get("model_optimizer", ""),
        "hypothesis": os.environ.get("HYPOTHESIS", ""),
    })

    # Hard-fail if any golden case regressed.
    if golden_failures > 0:
        print(
            f"GOLDEN REGRESSION: {golden_failures} golden case(s) failed — revert this cycle.",
            file=sys.stderr,
        )
        return 3

    return 0


if __name__ == "__main__":
    sys.exit(main())
