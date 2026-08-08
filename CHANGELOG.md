# Changelog

All notable user-facing changes are tracked here.

## 0.3.2 — 2026-08-08

### Changed

- **The web console is a question, not a flag you have to remember.** smith
  asks once on the first interactive run whether to serve it, saves the
  answer to `[web] enabled`, and never asks again — after that the console
  comes up with every session and no extra command is needed. `--web` still
  forces it on for a single run, and answering no is a real answer that stops
  the asking. Headless runs neither ask nor serve.

### Fixed

- **The context gauge told the truth on a gateway.** Every 9Router session
  reported a 32,768-token window whatever it was talking to: the catalogue
  warm covered OpenRouter and Ollama and skipped the gateway. It now reads
  the gateway's own `/v1/models` (`capabilities.contextWindow`) and resolves
  a model named the way its vendor names it — `nvidia/nemotron-…` — through
  the single catalogue entry that serves it. The gauge was the visible half;
  the expensive half was auto-compaction firing at a fraction of a window
  four times larger than smith believed, summarising conversations that
  still fit.

- **New sessions are named with a UUID, not the process id.** Every session
  was filed under `local-<pid>`: the id is allocated before the first turn
  (the scratch directory needs one) and `ensure_session` files the session
  under whatever it was handed, so `create_session` — the only thing that
  minted a UUID — was never reached. Pids are recycled, so two unrelated
  conversations could land in one row and have their histories merged.
  Existing `local-*` sessions keep resuming.

- **The sidebar sits on the terminal's edge again**, instead of riding inside
  the centred column with a field of empty cells to its right, and the
  document column now grows with the terminal (up to 160 columns) rather than
  holding a hundred while a full-screen window goes to waste.

- **The splash's box closes.** One row of the banner art carried a stray
  space past its right border, and a centred paragraph centres each row by
  its own width — so that row sat a cell off and broke the frame.

## 0.3.1 — 2026-08-07

### Added

- **The web console.** `smith --web` serves the running session to a browser
  on `127.0.0.1`, behind a per-run token shown in the TUI chrome: the live
  transcript over SSE (each frame is one `--output-format stream-json` line,
  verbatim), a composer that submits when idle and interjects mid-turn, the
  permission and question prompts, the task board, and the project's session
  history. First answer wins across frontends — approving in the browser
  closes the TUI's modal and vice versa. Off by default; `[web]` in the
  config (`enabled`, `port`, `open_browser`) or `--web`/`--web-port` per
  run; headless runs never start it. Design record: `docs/web-console.md`.

- **The task checklist grew into a board.** `write_tasks` now accepts
  `blocked` (with a one-line `blocked_reason`) and `review` statuses, and
  optional stable `id`s the model echoes back; smith stamps `updated_at` at
  receipt (never model-supplied) and persists the board per session, so
  `--resume` restores it stamps-and-all instead of re-scanning history.

### Changed

- **stream-json, two compatibility notes.** New `AgentEvent` variants
  `permission_resolved` and `question_resolved` join the contract (emitted
  by the interactive ask broker; never in headless output today — but treat
  unknown `type`s as skippable). `tasks_updated` payloads may now carry
  `status: "blocked" | "review"` plus optional `id`/`blocked_reason`/
  `updated_at` fields whenever the model uses them; consumers matching
  `status` exhaustively need the two new arms. A task using none of the new
  fields serializes byte-identically to before.

### Fixed

- **`smith --version` told the truth again, and `smith update` stopped looping.**
  Tags `v0.2.1`, `v0.2.2` and `v0.2.3` were released without bumping
  `[workspace.package] version`, which is what `env!("CARGO_PKG_VERSION")` bakes
  into the binary. Every one of those releases therefore reported `smith 0.2.0`
  — and because `smith update` compares the latest tag against that baked-in
  number, it saw a newer release, installed it, still reported the old version,
  and offered the same update again on the next run. If you installed
  0.2.1–0.2.3 you have the right binary; it was only ever describing itself
  wrongly. The release workflow now fails when the tag and the manifest
  disagree, so this cannot ship again.

### Added

- **Prerequisites are installed rather than reported.** `smith setup` opens on a
  `Runtimes` section that resolves what this configuration actually needs —
  Node for the 9router gateway, the gateway itself, Chrome for Testing,
  and any Ollama model named in the config — asking before each download.
  `smith doctor --fix` applies the same resolver without the wizard.
  Requirements are read from `[fallback] providers` as well as the primary
  provider, so a gateway that only appears in the fallback chain is set up
  before a turn reaches for it rather than when one does.

- **`smith uninstall`**, with `--yes` and `--keep-config`. Prints what it will
  remove and how much of it there is before removing anything, refuses any path
  that is not inside `~/.smith` or the running binary, and asks about your API
  keys separately from everything else. Per-project `.smith/` directories are
  named rather than hunted for: it prints the `find` that lists them and leaves
  them alone.

### Changed

- **Session history moved out of your projects.** It now lives in
  `~/.smith/projects/<name>-<hash>/sessions.db` instead of
  `<project>/.smith/sessions.db`, so running smith somewhere no longer leaves a
  multi-megabyte database in that directory. It is still per project — another
  project's conversations never appear in `/resume`. An existing database is
  moved the first time you run smith in that project, and smith says so when it
  does. What stays in `<project>/.smith/` is the data that *is* the project's:
  `/rewind` checkpoints, staging, and the session scratch directory, which the
  model is told about as a path inside the jail writes are confined to.

- **A Node the machine already has is used when it is new enough.** smith
  checks the Node on `PATH` against 9router's own floor (`>=18`) instead of
  taking the first one it finds on faith, and only downloads its private
  Node 24.19.0 when there is nothing usable. Previously a too-old Node was
  launched anyway and failed with whatever the gateway happened to print.

## 0.2.0 — 2026-08-06

### Added

- **Free providers with automatic fallback**, so a fresh install has working
  AI at no cost:
  - **OpenRouter** — one free key unlocks the `:free` models. smith drives the
    best free tool-capable one and sends the rest as a server-side chain
    (`route: "fallback"`), so a model hitting its limit is replaced inside the
    same request.
  - **9Router** — a local gateway, auto-installed by `smith setup` along with
    a private Node.js under `~/.smith/runtime/`, fanning out to 40+ upstream
    providers with its own internal fallback.
  - When the OpenRouter *account* quota dies mid-session, smith itself falls
    over to the next entry in `[fallback] providers` without losing the
    conversation. The handover is shown as it happens; the context gauge and
    compaction follow the new model's window; costs are recorded under the
    provider and model that actually served.
  - `smith setup` now opens with the two free options.
- Behavioral evals (`evals/`) for the halves of acceptance criteria #5 and #6
  that `cargo test` deliberately does not assert, reported as a rate per model
  rather than pass/fail. First run recorded under `evals/results/`.

### Fixed

- **Ollama context windows were assumed to be 4096.** For a cloud model with a
  262144-token window that is wrong by a factor of 64 — and it is not
  cosmetic, because auto-compaction fires against it: the conversation was
  being summarised away every couple of rounds, so the model kept forgetting
  what it had just searched for. smith now asks Ollama's own `/api/show`.
- **The context gauge hid an over-full window**, printing `max(window, used)`
  so 7.4k against a 4.1k window read as "100% 7.4k/7.4k" — a full window,
  which is normal — instead of 181% of a window detected wrong.
- **Every Ollama session was recorded as an OpenAI one.**
  `OpenAiProvider::id()` was hardcoded; it prices turns and labels the
  `turns.provider` column. Stored costs were always right; only the label was
  wrong, and no migration is needed.
- **`@path` completion ignored `.gitignore` outside a git repository**, so a
  non-repo project offered build artifacts.
- **The home directory could become the project root.** `~/.smith` is a root
  marker and always exists, so any project without its own `.git` resolved to
  `$HOME` — loading `~/SMITH.md` twice and widening the `@import` jail to the
  whole home directory.
- `smith setup` could fail to verify a freshly downloaded browser with "Text
  file busy": between `fork` and `exec`, a subprocess started by another
  thread holds an inherited write descriptor to the binary.

### Changed

- Answers are pinned to the language the user wrote in. Replies used to come
  back in English when every source read along the way was English.
- Settled knowledge is answered directly instead of searched for.
- A message typed mid-turn is delivered *into* the running turn at its next
  round boundary, so it can redirect work in flight; anything not taken by
  the turn's end is sent as its own turn.
- Consecutive searches collapse into one card with a row per query.

### Added

- Terminal UI:
  - The sidebar has tabs (`Session` / `Tasks` / `Vitals`); `Ctrl+B` hides it
    and hands its columns back to the transcript, `Shift+Tab` cycles the tabs.
    Stacked in one column, the four sections did not fit a 24-row terminal.
  - `/usage` and `/mcp` render as real tables in a scrollable panel instead of
    flat transcript lines.
  - `Ctrl+L` opens a diagnostics panel. Until now no `tracing` subscriber was
    installed anywhere in the workspace, so every warning the MCP client and
    the provider adapters emitted was discarded; they are now kept in memory
    and mirrored to `~/.smith/logs/smith.log`.
  - Syntax highlighting for fenced code blocks in eleven languages, with no
    new dependency.
  - Themes: `dark`, `light` and `high_contrast`, selectable with `--theme` or
    `[theme] name`, with per-token hex overrides under `[theme.colors]`. Every
    preset is asserted to meet WCAG AA contrast.
  - Prompt history on the arrow keys, seeded on `--resume` from the session's
    own messages; `@path` completion; mouse wheel and click.
  - `[keys]` remaps the five discretionary bindings — `Ctrl+B` is tmux's
    default prefix, so on a stock tmux the old binding could not be pressed
    at all.
- Accessibility release work:
  - `--ascii` forces ASCII-only TUI glyphs without mutating `SMITH_ASCII`.
  - `--plain` uses the existing headless frontend for linear, screen-reader
    friendly output with no colour.
  - `TERM=dumb` automatically avoids the TUI path.
  - The TUI now restores terminal state on ordinary error exits as well as
    panic unwinds.
  - A PTY regression test verifies an induced panic restores the alternate
    screen, the cursor, bracketed paste and mouse capture — and that it does
    so *before* printing the panic, so the message is not swallowed by the
    screen it is leaving.
- User extension documentation for subagents, custom slash commands, skills,
  personas, hooks, and SearXNG.
- Release/packaging documentation plus install/Homebrew/Scoop templates.

### Changed

- The 80x24 ASCII render test now asserts the rendered buffer is entirely
  ASCII, matching the release acceptance criterion directly.
- Session cost is reported by the agent from the per-turn figures recorded
  when each turn ran, instead of being recomputed in the TUI from a second
  price table. A resumed session used to display today's prices applied to the
  whole session's tokens, which disagreed with what it had actually spent.
  Turns billed against a model with no known price are now reported beside the
  total rather than silently omitted from it.
- An open modal no longer counts as animation. A permission prompt used to
  redraw the whole frame about eight times a second while waiting for the
  user, with a spinner claiming work was in progress.

### Fixed

- `smith setup` could fail to verify a freshly downloaded browser with
  "Text file busy": between `fork` and `exec`, a subprocess started by another
  thread holds an inherited write descriptor to the binary, and `execve`
  refuses while it is open. The probe now retries.

## 0.1.0

First public release candidate series.

Highlights from the development history:

- Multi-crate Rust workspace with a pure `smith-core` agent loop.
- TUI chat interface with streaming markdown, permission modals, plan gate,
  usage/context indicators, compact 80x24 layout, and per-tool activity cards.
- Providers for Anthropic, OpenAI, and Ollama-compatible local models.
- Built-in tools for file reads/writes/edits, globbing, grep, shell commands,
  web search/fetch, task checklists, and interactive questions.
- MCP client support over stdio, HTTP/SSE, resources, prompts, and `/mcp`.
- Session persistence with resume, continue, fork, export, `/goal`, and
  `/rewind` checkpoints for file changes.
- Custom commands, skills, personas, hooks, SearXNG/Tavily/Exa/Bing/Google News
  search tiers, and Chromium provisioning via `smith setup` / `smith doctor`.
