---
description: Add a feature: survey existing conventions, plan with write_tasks, implement in small verified increments.
---

# Workflow

1. State the acceptance criteria: what must be true when the feature is
   done, observable from outside the code. If the request allows several
   materially different readings, call `ask_user` once with three concrete
   options. At most one question — then commit to an interpretation and say
   which one.
2. Survey before designing. Find the most similar existing feature and read
   how it is built: `grep` for related names, `glob` for likely modules,
   `read_file` the one or two files that define the pattern. If the survey
   spans many files and you only need the shape of the answer, delegate it
   with `task`. The existing pattern is your template — a feature that
   mirrors its neighbors is reviewable; an alien one is a liability.
3. Find the project's quality gates (test/lint/format commands in CI config,
   `Makefile`, `package.json`, `Cargo.toml`, `pyproject.toml`, README). You
   will run them in step 6.
4. Plan with `write_tasks`: the full list of steps, smallest useful vertical
   slice first — the thinnest end-to-end path that makes one acceptance
   criterion observable. Wiring a stub through the real path beats building
   a complete layer nobody calls yet.
5. Implement one task at a time. Before each mutation, mark the task
   `in_progress`; after verifying it, mark it `completed` — or `review`
   when it is done but awaits the user's judgement, or `blocked` (with the
   reason) when it cannot proceed. Verify means: run
   the relevant test or command with `run_bash` and read its output — not
   "the code looks right". Reuse existing helpers instead of writing new
   ones; never add a dependency without saying so in the summary.
6. When all tasks are complete, run every quality gate from step 3.
7. If the project's conventions require tests for new behavior (they almost
   always do — look at how the neighboring feature is tested), the feature's
   tests are part of the feature, not an optional extra.

# Guardrails

- Do not widen scope: implement what was asked, mention what you chose not
  to do. "It would also be nice to…" goes in the summary as a suggestion,
  not in the diff.
- Do not restructure existing code to make room for the feature unless the
  feature is impossible without it — and then say so first.
- If an acceptance criterion turns out to be unreachable (missing API,
  conflicting constraint), stop and report it instead of shipping something
  adjacent to what was asked.

# Definition of done

- Every acceptance criterion from step 1 is observably true.
- Every task in the list is `completed`; every quality gate is green.
- The summary names each file changed and any new dependency, and states
  which interpretation you implemented if the request was ambiguous.
