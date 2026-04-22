# `taskfast-agent` SKILL.md autoresearch

Karpathy-style autoresearch loop for `skills/taskfast-agent/`. Locked eval + scorer; optimizer agent may only edit the skill bundle.

## Quick start

```bash
cd evaluation/skill-taskfast-agent
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
export ZAI_API_KEY=...                # required
# optional:
export ZAI_ENDPOINT_STYLE=anthropic    # default; openai requires paas/v4 billing bucket
export AGENT_MODEL=glm-5-turbo
export OPTIMIZER_MODEL=glm-5.1

# Baseline (3 runs, records per-run rows)
python run_eval.py --baseline --runs 3
python score.py

# Single case (debugging)
python run_eval.py --case-id golden_worker_e2e
python score.py
```

## Files

| File | Mutability | Purpose |
|---|---|---|
| `program.md` | **locked** | Driver instructions for the optimizer agent (human or Claude Code session drives cycles — no automated runner script). |
| `eval_cases.jsonl` | **locked** | 16 scenarios (golden / edge / retry / ordering / nonretryable / duplicate). |
| `run_eval.py` | **locked** | Harness — loads skill bundle, prompts GLM, records JSON plans. |
| `score.py` | **locked** | Weighted scorer. Lower is better. |
| `baseline_skill/` | snapshot | Pre-optimization bundle copy (for rollback comparison). |
| `traces/` | append-only | Per-run JSON logs. |
| `results.tsv` | append-only | Experiment history. |

## Mutable (optimizer-editable) surface

- `skills/taskfast-agent/SKILL.md`
- `skills/taskfast-agent/reference/*.md`

Everything in this `evaluation/` directory is immutable.

## Providers

z.ai exposes GLM via two API personas:

- **Anthropic-compat** — `https://api.z.ai/api/anthropic` (default; works with this account's flat-plan bucket).
- **OpenAI-compat** — `https://api.z.ai/api/paas/v4` (preferred for native `response_format` JSON mode, but requires paas/v4 billing package — empirical 2026-04: returns 1113 "Insufficient balance" on flat plan).

Swap via `ZAI_ENDPOINT_STYLE`. Anthropic path has no native JSON mode — `_safe_json` strips ` ```json ``` ` fences automatically (glm-5.1 likes to wrap).

## Convergence

Stop when 5 consecutive cycles show no score improvement past `2 × stdev` of the 3-run baseline, and golden-subset failures remain at 0. No budget cap.

## Regeneration note

`baseline_skill/` is captured once before the first optimizer cycle. Re-capture only if you intentionally reset the baseline:

```bash
rm -rf baseline_skill && cp -r ../../skills/taskfast-agent baseline_skill
```
