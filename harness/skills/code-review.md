---
description: Review a diff or branch: correctness, tests, security, consistency; report ranked findings with file:line.
---

# Workflow

1. Establish the scope with `run_bash`: `git status`, `git log --oneline
   -10`, and the diff itself (`git diff <base>...HEAD` for a branch,
   `git diff` for uncommitted work). Everything outside this diff is out of
   scope — note pre-existing problems in one line at most, do not review
   them.
2. Understand the intent: read the commit messages, and state in one
   sentence what the change is trying to do. A review that doesn't know the
   goal can only nitpick.
3. For each hunk, read the full surrounding context with `read_file` — the
   whole function, and the callers if the contract changed (`grep` for the
   callers). A hunk that looks fine in isolation is where bugs hide.
4. Check in this order, most severe first:
   a. **Correctness** — logic errors, edge cases (empty, zero, one, max,
      unicode, concurrent), error paths that swallow or mis-handle failures,
      off-by-one, broken invariants.
   b. **Security** — injection (shell, SQL, path), unvalidated input
      crossing a trust boundary, secrets in code or logs, paths escaping a
      jail, model- or user-controlled data reaching a dangerous sink.
   c. **Tests** — does the change's observable behavior have a test that
      would fail without the change? Are the error paths tested, or only the
      happy path?
   d. **Consistency** — does it match the codebase's idioms, naming, error
      handling, and module boundaries? Does it duplicate an existing helper
      (`grep` before claiming it does)?
   e. **Simplification** — only where it changes what a maintainer would do:
      dead code, needless abstraction, a 20-line block that is one stdlib
      call.
5. Verify every suspicion before reporting it. Read the callee, the caller,
   or the test that proves you wrong. A false finding costs the reviewer's
   credibility and the author's time.

# Report format

- Findings ranked most severe first. Each one: `file:line`, one-sentence
  claim, and the concrete failure scenario (input/state → wrong outcome).
  A finding without a failure scenario is an opinion — label it as such.
- End with what the change does well, in one or two lines, and an explicit
  verdict: ready, ready with nits, or needs changes.

# Guardrails

- No formatting or style nits in a project that has a formatter/linter —
  the gate will catch them; you are here for what the gate cannot see.
- Do not propose rewrites of the author's approach when their approach
  works. Review the change that was made, not the change you would have
  made.
- Read-only: a review changes no files unless the user asked you to also
  fix what you find.
