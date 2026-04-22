"""
Locked harness — cheap-proxy autoresearch eval for the taskfast-agent skill.

For each scenario in eval_cases.jsonl:
  1. Load the current skill bundle (SKILL.md + reference/*.md) as system prompt.
  2. Prompt the agent-under-test model (default glm-5-turbo) with the scenario.
  3. Expect strict JSON response:
       {"calls": [{"cmd": "...", "args": {...}, "on_error": "..."}, ...],
        "final_state": "...",
        "reasoning": "..."}
  4. Grade the declared call sequence against `expected` without running the CLI.

DO NOT EDIT scoring categories or `expected`-field semantics. Locked per program.md.

Env:
  ZAI_API_KEY              required
  ZAI_ENDPOINT_STYLE       openai | anthropic   (default: anthropic — paas/v4 bucket may be billing-gated)
  AGENT_MODEL              default: glm-5-turbo
  OPTIMIZER_MODEL          default: glm-5.1   (recorded in trace; not used here)

Usage:
  python run_eval.py                    # one eval pass
  python run_eval.py --baseline --runs 3   # multi-run baseline
  python run_eval.py --case-id <id>     # single case
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import uuid
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SKILL_ROOT = REPO / "skills" / "taskfast-agent"
CASES_PATH = HERE / "eval_cases.jsonl"
TRACE_DIR = HERE / "traces"
LATEST_TRACE = TRACE_DIR / "latest_eval.json"

OPENAI_BASE = "https://api.z.ai/api/paas/v4"
ANTHROPIC_BASE = "https://api.z.ai/api/anthropic"

SYSTEM_WRAPPER = """You are an autonomous TaskFast marketplace agent. Follow the skill bundle strictly.

For the scenario you receive, respond with a STRICT JSON object (no prose before or after, no markdown fences, no commentary) describing the sequence of `taskfast` CLI calls you would make, the expected final state, and brief reasoning.

Schema:
{
  "calls": [
    {"cmd": "taskfast ...", "args": {"flag": "value"}, "on_error": "retry|stop|skip|verify"}
  ],
  "final_state": "string describing terminal state",
  "reasoning": "one short paragraph"
}

Rules:
- Plan only calls justified by the skill bundle.
- Respect ordering / retry / verify-before-success rules from the bundle.
- Inject `on_error` semantics for every mutating call that could fail.
- Do not fabricate task_ids or bid_ids the prior_state did not supply.
- When a scenario says an error is injected, your plan should show how you would react to it.

OUTPUT ONLY THE JSON OBJECT. No text before it, no text after it, no code fences.

Skill bundle:
---BEGIN SKILL---
{skill}
---END SKILL---
"""

SUCCESS_KEYWORDS = (
    "success", "complete", "disbursed", "settled", "ready", "assigned",
    "under_review", "payment_disbursed", "confirmed",
)


def load_skill_bundle() -> str:
    parts = [(SKILL_ROOT / "SKILL.md").read_text()]
    ref = SKILL_ROOT / "reference"
    for p in sorted(ref.glob("*.md")):
        parts.append(f"\n\n# {p.name}\n\n{p.read_text()}")
    return "\n".join(parts)


def load_cases() -> list[dict]:
    cases: list[dict] = []
    for line in CASES_PATH.read_text().splitlines():
        line = line.strip()
        if line:
            cases.append(json.loads(line))
    return cases


_FENCE_RE = re.compile(r"^```(?:json)?\s*\n?(.*?)\n?```\s*$", re.DOTALL)
_STATE_NORM_RE = re.compile(r"[^a-z0-9]+")


def _normalize_state(s: str) -> str:
    """Lowercase, collapse non-alphanumeric runs to underscore.

    Lets grader treat 'payment disbursed', 'payment_disbursed', and
    'Payment-Disbursed' as the same terminal state.
    """
    return _STATE_NORM_RE.sub("_", s.lower()).strip("_")


_STATE_STOPWORDS = {
    "and", "or", "but", "the", "a", "an", "with", "of", "in", "on",
    "at", "to", "for", "is", "are", "was", "were", "no", "not",
}


def _final_state_match(expected: str, actual: str) -> bool:
    """Match `expected` slug against verbose `actual` prose.

    Strategy (in order of leniency):
      1. Bidirectional substring on normalized form.
      2. Token-subset: every meaningful expected token appears either
         in normalized actual OR in its underscore-collapsed form
         (lets 'rediscover' match 're_discover' / 're-discover').
    """
    if not expected:
        return True
    if not actual:
        return False
    e = _normalize_state(expected)
    a = _normalize_state(actual)
    if e in a or a in e:
        return True
    a_collapsed = a.replace("_", "")
    e_tokens = [
        t for t in e.split("_")
        if len(t) >= 3 and t not in _STATE_STOPWORDS
    ]
    return bool(e_tokens) and all(t in a or t in a_collapsed for t in e_tokens)


def _safe_json(text: str) -> dict:
    """Parse model output; strip code fences; return failure-shaped plan on non-JSON.

    Anthropic-compat path has no native `response_format` — some GLM variants
    (e.g. glm-5.1) wrap JSON in ```json ... ``` despite prompt instructions.
    """
    stripped = text.strip()
    m = _FENCE_RE.match(stripped)
    if m:
        stripped = m.group(1).strip()
    try:
        return json.loads(stripped)
    except json.JSONDecodeError as e:
        return {
            "calls": [],
            "final_state": "",
            "reasoning": f"json_decode_error: {e.msg} — raw={text[:200]!r}",
        }


def call_model(system: str, user: str, model: str, endpoint: str) -> dict:
    """Dispatch to z.ai endpoint; fall back from OpenAI path to Anthropic if asked."""
    if endpoint == "openai":
        from openai import OpenAI
        client = OpenAI(api_key=os.environ["ZAI_API_KEY"], base_url=OPENAI_BASE)
        resp = client.chat.completions.create(
            model=model,
            messages=[
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            response_format={"type": "json_object"},
            temperature=0.2,
        )
        return _safe_json(resp.choices[0].message.content)
    if endpoint == "anthropic":
        from anthropic import Anthropic
        client = Anthropic(api_key=os.environ["ZAI_API_KEY"], base_url=ANTHROPIC_BASE)
        resp = client.messages.create(
            model=model,
            system=system,
            messages=[{"role": "user", "content": user}],
            max_tokens=8192,
            temperature=0.2,
        )
        text = resp.content[0].text if resp.content else "{}"
        return _safe_json(text)
    raise ValueError(f"unknown endpoint style: {endpoint}")


def user_prompt(case: dict) -> str:
    return json.dumps({
        "scenario_id": case["id"],
        "category": case["category"],
        "user_goal": case["input"]["user_goal"],
        "prior_state": case["input"].get("prior_state", {}),
        "injected_events": case["input"].get("injected_events", []),
    })


def grade_case(case: dict, plan: dict) -> dict:
    """Score a single plan against the scenario's `expected` block.

    Returns a dict the scorer consumes: counts, required_* flags, passed.
    """
    expected = case.get("expected", {})
    calls = plan.get("calls", [])
    cmd_sequence = [c.get("cmd", "").strip() for c in calls]
    final_state = plan.get("final_state", "")

    counts = {
        "fatal": 0, "wrong_order": 0, "schema_error": 0, "invalid_args": 0,
        "timeout": 0, "nonretryable_retried": 0, "missing_retry": 0,
        "excess_retry": 0, "recoverable": 0,
    }

    required_prefix = expected.get("required_calls_prefix", [])
    ptr = 0
    for cmd in cmd_sequence:
        if ptr < len(required_prefix) and cmd.startswith(required_prefix[ptr]):
            ptr += 1
    prefix_ok = ptr == len(required_prefix)
    if not prefix_ok:
        counts["wrong_order"] += 1

    for forbidden in expected.get("forbidden_calls", []):
        for cmd in cmd_sequence:
            if cmd.startswith(forbidden):
                counts["fatal"] += 1

    for pair in expected.get("forbidden_orders", []):
        a, b = pair
        idx_a = next((i for i, c in enumerate(cmd_sequence) if c.startswith(a)), None)
        idx_b = next((i for i, c in enumerate(cmd_sequence) if c.startswith(b)), None)
        if idx_a is not None and idx_b is not None and idx_a < idx_b:
            counts["wrong_order"] += 1

    for forbidden_behavior in expected.get("forbidden_behaviors", []):
        if forbidden_behavior == "retry_422" or forbidden_behavior == "retry_400":
            for call in calls:
                if call.get("on_error") == "retry":
                    counts["nonretryable_retried"] += 1

    verify_map = expected.get("must_verify_after", {})
    for trigger, verifiers in verify_map.items():
        for i, cmd in enumerate(cmd_sequence):
            if cmd.startswith(trigger):
                tail = cmd_sequence[i + 1 : i + 1 + 5]
                if not any(any(t.startswith(v) for t in tail) for v in verifiers):
                    counts["missing_retry"] += 1  # reused bucket: missing follow-up

    retryable = set(expected.get("retryable_errors", []))
    for call in calls:
        args = call.get("args", {}) or {}
        for v in args.values():
            if isinstance(v, str) and ("YOUR_" in v or "FAKE" in v or "PLACEHOLDER" in v):
                counts["invalid_args"] += 1
                break

    expected_final = expected.get("final_state", "")
    final_ok = _final_state_match(expected_final, final_state)

    final_state_lc = final_state.lower()
    claims_success = any(kw in final_state_lc for kw in SUCCESS_KEYWORDS)
    claimed_success_but_failed = claims_success and (
        counts["fatal"] > 0 or not final_ok or not prefix_ok
    )

    passed = (
        prefix_ok
        and final_ok
        and counts["fatal"] == 0
        and counts["wrong_order"] == 0
        and counts["schema_error"] == 0
        and counts["nonretryable_retried"] == 0
        and not claimed_success_but_failed
    )

    return {
        "id": case["id"],
        "category": case["category"],
        "passed": passed,
        "counts": counts,
        "required_final_state_ok": final_ok,
        "required_api_sequence_prefix_ok": prefix_ok,
        "claimed_success_but_failed": claimed_success_but_failed,
        "plan": plan,
    }


def run_once(model: str, endpoint: str, only_case: str | None = None) -> dict:
    skill = load_skill_bundle()
    system = SYSTEM_WRAPPER.replace("{skill}", skill)

    cases = load_cases()
    if only_case:
        cases = [c for c in cases if c["id"] == only_case]
        if not cases:
            raise SystemExit(f"no case with id {only_case}")

    results = []
    for c in cases:
        prompt = user_prompt(c)
        try:
            plan = call_model(system, prompt, model=model, endpoint=endpoint)
        except Exception as e:  # noqa: BLE001
            plan = {"calls": [], "final_state": "", "reasoning": f"harness_error: {e}"}
        graded = grade_case(c, plan)
        results.append(graded)
        print(f"[{c['id']}] passed={graded['passed']} counts={graded['counts']}", flush=True)

    return {
        "run_id": f"{int(time.time())}-{uuid.uuid4().hex[:6]}",
        "model_agent": model,
        "model_optimizer": os.environ.get("OPTIMIZER_MODEL", "glm-5.1"),
        "endpoint": endpoint,
        "cases": results,
    }


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--baseline", action="store_true", help="Run baseline N times in a row")
    p.add_argument("--runs", type=int, default=1)
    p.add_argument("--case-id", default=None)
    p.add_argument("--write-trace", action="store_true", default=True)
    args = p.parse_args()

    if "ZAI_API_KEY" not in os.environ:
        print("ERROR: set ZAI_API_KEY", file=sys.stderr)
        return 2

    endpoint = os.environ.get("ZAI_ENDPOINT_STYLE", "anthropic")
    model = os.environ.get("AGENT_MODEL", "glm-5-turbo")

    TRACE_DIR.mkdir(parents=True, exist_ok=True)
    runs = args.runs if args.baseline else 1
    for i in range(runs):
        trace = run_once(model=model, endpoint=endpoint, only_case=args.case_id)
        run_id = trace["run_id"]
        out = TRACE_DIR / f"{run_id}.json"
        out.write_text(json.dumps(trace, indent=2))
        LATEST_TRACE.write_text(json.dumps(trace, indent=2))
        print(f"wrote {out}", flush=True)

    return 0


if __name__ == "__main__":
    sys.exit(main())
