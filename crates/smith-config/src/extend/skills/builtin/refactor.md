---
description: Restructure without changing behavior: green baseline first, one small transformation at a time, verify each.
---

# Workflow

1. Establish the baseline FIRST: find the project's test command and run it
   with `run_bash`. Record what is green.
   - If the tests are already failing, STOP and report the failures. Never
     refactor on red: you would lose the only signal that separates "my
     restructuring broke it" from "it was already broken".
   - If the code you are about to restructure has no test coverage at all,
     say so and ask whether to add characterization tests first — a refactor
     without a safety net is a rewrite wearing a refactor's name.
2. Plan with `write_tasks`: one mechanical transformation per task (extract
   this function, move that module, rename this type, invert that
   dependency). Small enough that if a step breaks the tests, the cause is
   the step.
3. Execute one task at a time:
   - Read the code you are about to move or change (`read_file` — the gate
     will refuse blind overwrites anyway).
   - Apply the transformation. Use `multi_edit` for multi-site changes
     within one file (all-or-nothing); `edit_file` with `replace_all` for a
     rename that is unambiguous in that file.
   - Run the tests. Green → mark the task `completed` and move on.
     Red → fix or revert THIS step before touching the next. Never carry a
     red bar forward.
4. After the last task, run all quality gates (tests, lint, format), then
   review the full diff with `run_bash` (`git diff`) reading it only for
   accidental behavior change: swapped conditions, dropped early returns,
   changed defaults, reordered side effects.

# Guardrails

- Behavior and public API stay identical unless the user explicitly asked
  otherwise. "While I'm here" fixes, even correct ones, go in the summary as
  suggestions — mixing them in destroys the reviewer's ability to check the
  refactor by inspection.
- Do not change tests to make them pass. A test that must change under a
  pure refactor is evidence the change is not pure — say so.
- If a planned step turns out to require a behavior change, stop, report it,
  and let the user decide.

# Definition of done

- The exact same test set that was green in step 1 is green now.
- All quality gates pass; the diff has been read end to end.
- The summary lists each transformation performed and states explicitly
  that behavior is unchanged — or names the exception the user approved.
