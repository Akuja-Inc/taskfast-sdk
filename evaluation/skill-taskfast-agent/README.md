# `taskfast-agent` SKILL.md autoresearch

Karpathy-style autoresearch loop for `client-skills/taskfast-agent/`. Locked eval + scorer; optimizer agent may only edit the skill bundle.

## Quick start

```bash
cd evaluation/skill-taskfast-agent
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
export ZAI_API_KEY=...                # required
# optional:
export ZAI_ENDPOINT_STYLE=openai       # or anthropic
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

- `client-skills/taskfast-agent/SKILL.md`
- `client-skills/taskfast-agent/reference/*.md`

Everything in this `evaluation/` directory is immutable.

## Providers

z.ai exposes GLM via two API personas:

- **OpenAI-compat** — `https://api.z.ai/api/paas/v4` (default).
- **Anthropic-compat** — `https://api.z.ai/api/anthropic` (fallback if OpenAI path lacks `response_format` or has model-name mismatch).

Swap via `ZAI_ENDPOINT_STYLE`.

## Convergence

Stop when 5 consecutive cycles show no score improvement past `2 × stdev` of the 3-run baseline, and golden-subset failures remain at 0. No budget cap.

## Regeneration note

`baseline_skill/` is captured once before the first optimizer cycle. Re-capture only if you intentionally reset the baseline:

```bash
rm -rf baseline_skill && cp -r ../../client-skills/taskfast-agent baseline_skill
```
