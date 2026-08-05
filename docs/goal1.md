```text
# Goal: Smith agent slash commands (incremental delivery)

## Context
Smith is a terminal AI coding agent (Rust, multi-crate workspace). The chat loop,
tools, permissions, sessions, and MCP already work. UI improvements (idle screen,
sidebar, footer, layout) are done or in progress. Next focus: maximize
capabilities via slash commands, in the spirit of Claude Code / OpenCode.

## Language policy
- Long-term goal: make the project multi-language (i18n-ready UI strings,
  messages, and docs).
- For now: ship everything in English as the default open-source standard —
  code comments, user-facing TUI copy, `/help` text, errors, README, and commit
  messages. Do not introduce a second locale yet; when adding strings, prefer
  centralizing them so i18n can be layered later without rewriting call sites.

## Delivery principles
- Split work into small, independent packages.
- Deliver one command (or one minimal cluster) per coherent PR/commit.
- Each package must: compile, pass `cargo fmt` + `clippy -D warnings` +
  `cargo test --workspace`, update `/help`, and be usable in the TUI without
  depending on the next package.
- Prefer extending the existing slash dispatcher in `smith-tui`
  (`run_slash_command`) and persisting state in `smith-store`
  (`~/.smith/config.toml` and/or `.smith/`) when needed.
- Do not refactor architecture beyond what the current package requires.

## Suggested package order

### P1 — `/model`
Switch provider/model at runtime in the current session (beyond `--model` /
`smith setup model`).
- List available models for the current provider when possible.
- Persist the choice if the user confirms (or via an explicit flag).
- Show clear feedback in the footer/sidebar for the active model.

### P2 — `/permission`
Configure the dangerous-tool permission policy:
- `ask` (default): always prompt (current y / a / n modal).
- `session` / `allow-session`: allow for the rest of the session.
- `skip` / `yolo`: skip prompts (allow-all) — with an explicit risk warning.
- Persist preference in config; allow per-session override via the command.
- Document modes in `/help` and the README.

### P3 — `/usage`
Show session/project usage: tokens in/out, estimated cost (if pricing exists),
requests, tools invoked. Reuse sidebar stats when possible; print a readable
summary in the transcript (optional detail view).

### P4 — `/plan`
Planning mode: the agent produces a structured plan (steps, risks, files)
before large changes. The plan is visible in the transcript (ideally
editable/approvable). Do not run destructive tools until the user confirms the
plan (still respecting `/permission`).

### P5 — `/goal`
Set a high-level objective for the session. Keep the goal as persistent context
(session DB or `.smith/goal.md`), track progress, and optionally break it into
subtasks. `/goal` with no args shows the current goal; `/goal clear` removes it.

### P6 — `/loop`
Run a prompt (or the active goal) in a loop with a stop condition:
- N iterations, or
- until the agent declares “done”, or
- until failure / cancel (Esc).
Each iteration must be visible in the transcript; cancellation must be safe.

### P7 — `/kanban`
Simple session/project task board (todo / doing / done), linked to `/goal` and
`/plan` when those exist. Minimal commands: list, add, move, clear. Persist in
`.smith/kanban.json` (or equivalent). Start with textual transcript output; a
dedicated TUI panel only if it fits without bloating scope.

### P8 — `/ultraplan`
Deep / multi-step planning: codebase research, alternatives, trade-offs, detailed
plan with acceptance criteria — heavier than `/plan`. May use more
read-only turns/tools; produces a persistent artifact (`.smith/ultraplan.md`)
and optionally feeds `/kanban` + `/goal`.

## Acceptance criteria (per package)
1. Slash command registered, with args documented in `/help` (English).
2. Predictable behavior with friendly errors (unknown command, missing args).
3. Persistent state where the feature needs continuity across sessions.
4. No regression in streaming chat, Esc-cancel, permission modal, or `/clear`.
5. Unit tests for pure logic (arg parsing, permission policy, goal/kanban/plan
   serialization); UI tests only if an existing pattern already covers them.

## Out of scope (for now)
- Broad visual redesign (covered by the UI work).
- New providers beyond the existing ones.
- MCP protocol rework.
- Features outside the commands above, except minimal shared foundation
  (e.g. slash-command registry, arg parsing).
- Full i18n implementation — English-only until a later dedicated package.

## How to work
Before each package: inspect the current code (slash dispatcher, config, agent
loop, events). Implement the smallest vertical slice. When a package is done,
stop and report what shipped + how to test it manually in the TUI; only then
move to the next package.
```