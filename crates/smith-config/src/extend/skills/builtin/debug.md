---
description: Diagnose an unknown failure: gather evidence, test one hypothesis at a time, instrument before guessing.
---

# Workflow

1. Capture the evidence exactly: the full error message, stack trace, exit
   code, and the command or input that triggers it. Reproduce it once with
   `run_bash` so you are debugging the failure, not the report of it. If it
   does not reproduce, that IS the finding — report what differs between
   your run and the report.
2. Read the code the evidence points at: `grep` for the error text or the
   frame at the top of the trace, `read_file` the region with
   `offset`/`limit`. Read what actually executes, not what you assume does.
3. State ONE hypothesis, out loud, in one sentence: "X fails because Y."
   A hypothesis you can't state is a guess you can't test.
4. Design the cheapest experiment that could FALSIFY it — in rough order of
   cost:
   - a `grep`/`read_file` that checks whether the code even does what the
     hypothesis assumes;
   - a targeted print/log or assertion, added temporarily;
   - a minimal reproduction script written to the scratch directory (never
     into the project tree);
   - `git bisect` or diffing against the last known-good state when the
     question is "what changed".
5. Run the experiment and read the result honestly:
   - Hypothesis falsified → record it in one line ("ruled out: Y — because
     Z") and go back to step 3 with the next hypothesis. This record is what
     stops you from circling.
   - Hypothesis confirmed → you have the cause. Remove any temporary
     instrumentation, then switch to the `fix-bug` workflow at its step 5
     (task list, failing test, minimal fix, gates).

# Guardrails

- One hypothesis at a time. Never apply a speculative fix to see if it
  helps — stacked speculative fixes are how a bug becomes two.
- Never conclude from evidence you didn't collect this session: "it's
  probably the cache" without an experiment is a guess wearing a diagnosis's
  name.
- If three consecutive hypotheses die, zoom out: re-read the evidence from
  step 1, question an assumption you marked as safe, or bisect instead of
  theorizing.
- Timebox honestly: if the cause resists diagnosis, report the ruled-out
  list and the strongest remaining lead — that report has value; a wrong fix
  has negative value.

# Definition of done

- Either the cause is identified and confirmed by an experiment (and the
  fix-bug workflow takes over), or the user has a report of what was ruled
  out, with the evidence, and what to try next.
