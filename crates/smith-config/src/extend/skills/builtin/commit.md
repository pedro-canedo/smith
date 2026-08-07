---
description: Create a git commit: inspect the diff, run quality gates, stage deliberately, match the repo's message style.
---

# Workflow

1. See what you are about to commit, with `run_bash`:
   `git status`, then `git diff` (and `git diff --staged` if something is
   already staged). Never commit changes you have not read — including
   changes you did not make: if the tree contains edits you don't recognize,
   stop and ask before sweeping them into your commit.
2. Run the project's quality gates (test, lint, format — from CI config,
   `Makefile`, or the manifest). A commit that reddens CI is worse than no
   commit. If a gate fails for a reason unrelated to your change, report it
   and ask whether to commit anyway.
3. Stage deliberately, by name: `git add <file> <file>`. Never blind
   `git add -A` or `git add .` — that is how secrets, scratch files, and
   unrelated edits end up in history. Check `git status` again after
   staging: the "to be committed" list must contain exactly what you intend.
4. Learn the repo's message convention from `git log --oneline -15`:
   conventional commits (`feat:`/`fix:`), plain imperative, ticket prefixes,
   language. Match it — the log's consistency is worth more than your
   preferred style.
5. Write the message:
   - Subject: imperative mood, ≤72 characters, says what the change does
     ("Add retry to session store", not "Added retries" or "fixes").
   - Body (when the why is not obvious from the subject): why the change was
     needed and any non-obvious decision. Not a file list — the diff is the
     file list.
6. Commit, then confirm with `git log -1 --stat` that the right files went
   in under the right message.

# Guardrails

- Never commit: secrets or credentials (grep the staged diff for keys,
  tokens, passwords if in doubt), generated artifacts the repo ignores,
  large binaries, or the scratch directory's contents.
- Never `push`, never amend or rebase published history, never `--force`,
  unless the user explicitly asked for that operation by name.
- One logical change per commit. If the tree holds two unrelated changes,
  stage and commit them separately — or ask.
- If there is nothing to commit, say so; do not manufacture an empty commit.

# Definition of done

- Gates green, staged set reviewed, message matches the repo's convention,
  `git log -1 --stat` confirmed and reported to the user.
