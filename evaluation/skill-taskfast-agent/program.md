# Optimizer program — `taskfast-agent` SKILL.md

You are the optimizer. Your goal: minimize the total error score of the locked evaluation set by editing the `taskfast-agent` skill bundle. Lower is better.

## Files

**You MAY edit:**

- `skills/taskfast-agent/SKILL.md`
- `skills/taskfast-agent/reference/BOOT.md`
- `skills/taskfast-agent/reference/WORKER.md`
- `skills/taskfast-agent/reference/POSTER.md`
- `skills/taskfast-agent/reference/STATES.md`
- `skills/taskfast-agent/reference/TROUBLESHOOTING.md`

**You MUST NOT edit:**

- `evaluation/skill-taskfast-agent/run_eval.py`
- `evaluation/skill-taskfast-agent/score.py`
- `evaluation/skill-taskfast-agent/eval_cases.jsonl`
- `evaluation/skill-taskfast-agent/program.md` (this file)

**You MUST NOT:**

- Weaken the grader or remove hard cases.
- Delete required-behavior clauses.
- Change the skill bundle's published output schema / fences.
- Optimize one case at the expense of the golden-path subset (regressing golden = revert, always).

## Workflow (per cycle)

1. Read the current bundle. Read `results.tsv` (last 5 rows) and the 3 most recent `traces/*.json` for failure patterns.
2. Form **one** hypothesis about what's causing the top-ranked failure category (fatal → wrong_order → schema → missing_retry → ...).
3. Propose a **single targeted edit** to the bundle. Single hypothesis, single commit, one file change where possible.
4. Run the eval: `python evaluation/skill-taskfast-agent/run_eval.py --write-trace`.
5. Run the scorer: `python evaluation/skill-taskfast-agent/score.py`.
6. Compare to the current best score.
   - Strict improvement past the noise threshold AND golden subset == 0 failures → **keep** the edit; append a `results.tsv` row.
   - Otherwise → **revert** the bundle to its prior state; append a row noting the rejected hypothesis.
7. Repeat. Stop when 5 consecutive cycles fail to improve past the noise threshold (convergence).

## Allowed optimization directions

- Tighten required API call ordering (explicit prerequisites).
- Split retryable vs non-retryable errors per endpoint class.
- Add explicit partial-state recovery steps (list-before-retry, idempotent vs not).
- Add "do not claim success until X verifies Y" guards.
- Add idempotency and duplicate-call prevention.
- Remove vague / contradictory wording.
- Add "do not proceed" guards when prerequisites are missing.

## Disallowed

- Adding vague prose ("be careful", "think about it").
- Adding contradictory instructions.
- Changing the `taskfast` CLI contract (commands, flags, envelope shape).
- Skipping required APIs to reduce errors artificially.

## Failure taxonomy (consult traces)

When a case fails, classify the agent's output under one of:

- `fatal` — final state is incoherent (e.g. claimed success but required_final_state_ok == false).
- `wrong_order` — mandatory prerequisite skipped (e.g. `task submit` before `task claim`).
- `schema_error` — malformed args / missing required field.
- `invalid_args` — well-formed but wrong value (e.g. impossible bid price).
- `missing_retry` — 429/503/timeout not retried when it should have been.
- `excess_retry` — non-retryable 4xx retried.
- `nonretryable_retried` — tried retry on 401/403/422/409 where the skill says don't.
- `timeout` — agent gave up / hung.
- `recoverable` — surfaced error cleanly but with suboptimal recovery.

## Commit discipline

Each optimizer cycle = one logical bundle change. Summarize the hypothesis in the `hypothesis` column of `results.tsv`. Example: `"tightened retry guidance to block 422 retries after seeing 3 nonretryable_retried failures on 422 validation_error"`.

## Convergence

Stop when:

- 5 consecutive cycles produce no score improvement past noise threshold (`2 × stdev` of 3-run baseline), and
- Golden subset remains at 0 failures.

Do not loop past that. Hand off to a human for final review of the bundle before commit.
