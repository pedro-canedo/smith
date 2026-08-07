---
description: Use the task tool well: when to delegate, how to write a self-contained subagent prompt, how to use the report.
---

# When to delegate

Call `task` when the answer requires reading or searching across MANY files
and you only need the conclusion — a survey ("how is error handling done
across the providers?"), a location hunt ("where does X get validated?"), an
inventory ("list every implementor of trait Y with file:line"). The
subagent's reads fill ITS context and are discarded; only its final report
enters yours. That asymmetry is the entire point — delegating a six-file
survey costs you a paragraph instead of six files.

Do NOT delegate:
- what one `grep` or one `read_file` answers — the subagent costs a full
  child turn; a single lookup is cheaper done directly;
- work that needs your conversation's context to interpret — the child sees
  none of it;
- anything requiring writes, commands, or questions — the child is
  read-only, cannot run `run_bash`, cannot `ask_user`, cannot delegate
  further. It reports; you act.

# Writing the prompt

The prompt is ALL the child sees. Write it as a work order to a competent
stranger with no history:

1. **Context** — the one or two facts from this conversation the child
   needs ("we are changing the retry policy in smith-provider").
2. **The exact question** — narrow and answerable. "Investigate the config
   system" produces an essay; "list every config key read in
   crates/smith-config/src/lib.rs with its default and file:line" produces
   an answer.
3. **Where to look** — starting paths, known symbol names, search terms you
   already know. Every hint saves the child's limited budget (it has ~30
   tool calls and 4 minutes).
4. **Report format** — demand concrete anchors: file paths, line numbers,
   symbol names, quoted snippets where exact text matters, and an explicit
   "could not determine" for gaps. Forbid process narration.

Two independent questions = two `task` calls (they can run in the same
round); one child given a compound mission splits its budget badly.

# Using the report

- Act on the report instead of re-reading everything it read — re-verifying
  every claim yourself cancels the savings. Spot-check only the one or two
  claims your next mutation depends on (`read_file` the exact line cited).
- A report marked partial (budget ran out) still contains real work: use
  what is there and delegate a NARROWER follow-up for the remainder, rather
  than repeating the same broad question.
- The user pays for the child's tokens and sees its progress lines — mention
  in your reply what the delegation found, not that delegation happened.
