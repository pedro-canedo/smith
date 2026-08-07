---
description: Fix a reported bug: reproduce it, locate the cause, fix minimally, prove it with a test, run the quality gates.
---

# Workflow

1. Restate the bug in one sentence: what happens, what should happen instead.
   If the report is too vague to reproduce, call `ask_user` once with three
   concrete interpretations. Do not start editing on a guess.
2. Find the project's test and build commands before touching anything:
   `glob` for `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`,
   `Makefile`, `.github/workflows/*`. The commands you find there are the
   quality gates for step 8.
3. Reproduce the bug with `run_bash` — run the failing command, the failing
   test, or a minimal script in the scratch directory. If you cannot
   reproduce it, STOP and report exactly what you tried and what you saw
   instead. Never "fix" what you could not observe.
4. Locate the cause. `grep` for the error message, the symptom's wording, or
   the function named in the stack trace. Then `read_file` the relevant
   region with `offset`/`limit` — not the whole file. Follow the data, not
   your first theory: read the code that actually runs.
5. If the fix will take 3 or more steps, call `write_tasks` with the full
   list before starting.
6. If the project has a test suite, write the test that fails because of the
   bug BEFORE writing the fix. Run it and watch it fail for the reported
   reason. This test is your proof and your regression guard.
7. Make the minimal fix with `edit_file`. Match the surrounding code's style,
   naming, and error handling. No drive-by refactors, no reformatting of
   untouched lines, no "while I'm here" changes — those belong in a separate
   request.
8. Verify: run the new test, then the full quality gates from step 2. All of
   them, not just the one nearest the change.

# Guardrails

- Two failed fix attempts in a row means your theory of the cause is wrong.
  Stop patching. Load the `debug` skill and diagnose properly.
- Never silence a symptom (catch-and-ignore, deleted assertion, widened type)
  without stating that this is what the change does.
- Touch only files involved in the cause and its test. If the diff grows
  beyond that, say why.

# Definition of done

- The original reproduction from step 3 now passes.
- The new test fails on the old code and passes on the new code.
- Every quality gate is green.
- You reported, in one short paragraph: the cause, the fix, and the files
  changed.
