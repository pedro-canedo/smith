---
description: Produce an approvable plan under /plan: explore read-only, numbered steps with files and risks, plain text.
---

# Workflow

1. Explore before you plan. Use only read-only tools — `grep`, `glob`,
   `read_file`, `list_dir`, and `task` for surveys that span many files.
   Every claim in the plan ("X lives in Y", "Z already handles this") must
   come from something you read this session, not from assumption. A plan
   built on guessed file names collapses at step one of execution.
2. Look for the existing pattern the change should follow: the most similar
   feature, the module that already solves half the problem, the helper that
   must be reused instead of rewritten. Name them in the plan with their
   paths.
3. Identify the quality gates (test/lint/format commands) — the plan's last
   step is always running them.
4. Write the plan, as plain text in your reply, with this shape:
   - **Context** — one short paragraph: what is being changed and why.
   - **Steps** — numbered, each one concrete enough to execute without
     re-deciding: the file(s) touched, what changes in them, and how that
     step is verified. Small steps: one logical change each.
   - **Files affected** — the list, with paths.
   - **Risks** — what could break, what is uncertain, what depends on an
     assumption you could not verify read-only (name the assumption).
   - **Out of scope** — what you are deliberately not doing, so approval
     means the same thing to both sides.
5. End the reply with the plan itself. No mutations of any kind while
   planning: the plan gate blocks them, and a plan that already started
   executing is not a plan.

# After approval

- Approval arrives through the UI (`/plan approve`), never as chat text.
  Never ask "should I proceed?" in chat — the approval UI is the only
  channel, and asking again stalls the turn.
- Once told the plan is approved: start immediately with the first concrete
  tool call of step 1. Use `write_tasks` to mirror the plan's steps if there
  are 3 or more. Do not re-litigate the plan; if execution reveals the plan
  was wrong somewhere, say so at that step and adapt visibly.

# Guardrails

- A plan is not a design essay. Alternatives you considered get at most one
  line each in Risks/Out of scope; the plan describes the ONE approach you
  recommend.
- If exploration shows the task is trivial (one obvious small edit), say so
  and propose the single step — padding a trivial change into a ceremony
  wastes the user's approval on nothing.
- If exploration shows the task is impossible or already done, that IS the
  plan's conclusion. Report it instead of planning around it.
