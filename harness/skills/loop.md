---
description: Looped work: pick the next increment, verify it, record state; emit LOOP_DONE only when truly done.
---

# Workflow

Each iteration of a looped task follows the same five moves:

1. **Re-read the completion criterion.** The loop was started with a
   definition of done. Every iteration begins by checking the current state
   against it — with tools (`run_bash` the tests, `grep` the remaining
   markers, count the items left), not from memory of the last iteration.
2. **Pick the smallest next increment** that moves toward the criterion.
   One increment per iteration: one file migrated, one test fixed, one item
   processed. Resist batching — a small verified step survives an
   interruption; a half-finished batch doesn't.
3. **Execute it** with the ordinary discipline: read before editing, match
   the surrounding style, minimal diff.
4. **Verify it** with `run_bash` before counting it: the relevant test, the
   build, the command that proves this increment landed. An unverified
   increment is not progress, it is deferred debugging.
5. **Record state** so the next iteration (or a fresh context) can resume
   without archaeology: keep `write_tasks` current — completed work marked,
   the next increment named as `pending`. The task list is the loop's
   memory; keep it truthful even when an increment failed (leave it
   `in_progress` and note the blocker in your reply).

# Termination

- Emit the sentinel `LOOP_DONE` when — and only when — the original
  completion criterion is verifiably satisfied: the check from step 1 passes
  and nothing remains in the task list.
- Never emit it out of fatigue, uncertainty, or "mostly done". Never ask
  whether to continue — the loop's contract is that you continue until done
  or genuinely blocked.
- **Blocked is different from done.** If an increment cannot proceed without
  input only the user can give (a credential, a decision, an external
  system down), say precisely what is blocking and what is needed — and do
  NOT emit `LOOP_DONE`. Do not invent an answer to keep the loop moving.

# Guardrails

- No scope creep across iterations: the criterion set at the start is the
  criterion. If the work reveals the criterion itself is wrong, report that
  rather than silently redefining done.
- Watch for a stuck loop: if two consecutive iterations produced no verified
  increment, stop repeating the same attempt — change approach (load the
  `debug` skill if it's a failure) or report the blocker.
- Each iteration's reply is short: what was done, what was verified, what is
  next. The task list carries the totals; prose repeats none of it.
