# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo build --workspace                    # build everything
cargo run -p smith-cli                     # run the TUI (dev)
cargo run -p smith-cli -- setup            # interactive provider/model wizard
cargo run -p smith-cli -- --provider ollama --model qwen2.5   # override provider/model for one run
cargo run -p smith-cli -- --resume <id>    # resume a saved session

cargo test --workspace                     # all tests
cargo test -p smith-tui                    # one crate
cargo test -p smith-tui app::tests::plan_gate_changed_event_syncs_state  # one test

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests live inline in `#[cfg(test)] mod tests { ... }` at the bottom of the
source file they cover — there are no separate test files. `cargo fmt`,
`cargo clippy -D warnings`, and `cargo test --workspace` must all pass before
committing.

## Architecture

`smith` is a terminal-based AI coding agent (Rust/ratatui), in the spirit of
Claude Code. It's a Cargo workspace of 7 crates with dependencies flowing one
way: `smith-core` defines traits and knows nothing about HTTP, SQLite, or
`ratatui`; every other crate implements against those traits.

- **`smith-core`** — domain types (`Message`, `ContentBlock`, `StreamEvent`),
  the `LlmProvider`/`Tool` traits, and the `Agent` orchestration loop
  (`agent.rs`). Read this crate first; `agent.rs::run_turn` is the center of
  the whole system.
- **`smith-provider`** — `LlmProvider` adapters: Anthropic, OpenAI, and
  Ollama (via the OpenAI-compatible adapter), including SSE stream parsing.
- **`smith-tools`** — built-in tools (`read_file`, `write_file`, `edit_file`,
  `list_dir`, `glob`, `run_bash`, `ask_user`) and the `ToolRegistry` that
  implements `smith_core::ToolExecutor`.
- **`smith-mcp`** — hand-rolled JSON-RPC-over-stdio MCP client; bridges
  remote MCP server tools into the same `Tool` trait as built-ins (default
  `PermissionClass::Dangerous`, same as `run_bash`).
- **`smith-store`** — global config (`~/.smith/config.toml`) and
  per-project session history (`.smith/sessions.db`, SQLite).
- **`smith-tui`** — the `ratatui`/`crossterm` terminal UI (chat pane, input
  box, permission/plan/question modals, sidebar). Never talks to
  `smith-provider` or `smith-tools` directly — only through the
  `Action`/`AgentEvent` channels.
- **`smith-config`** — layered configuration: global `~/.smith/config.toml`
  with `<project>/.smith/config.toml` merged over it, field by field. Split
  out of `smith-store` so reading a TOML file doesn't compile SQLite from C.
- **`smith-cli`** — binary entry point: CLI flags (`clap`), the system prompt,
  and `orchestrator.rs`, which owns the `Action` → `Agent` loop. Two frontends
  drive that same orchestrator over the same channel bundle: `smith_tui::run`
  and `headless.rs`. Headless is chosen by `-p`, by `--output-format`, or by
  stdout not being a terminal.

### The `Action` / `AgentEvent` loop

This is the pattern to understand before touching orchestration or TUI code:

1. User input in `smith-tui` (`app.rs::on_key`) produces an `Action`
   (`SubmitMessage`, `StartPlan`, `SwitchModel`, `SetPermissionPolicy`, ...).
2. `smith-tui::run` forwards it over a channel to the orchestrator
   (`smith-cli/main.rs`), which matches on it and drives `Agent::run_turn`.
3. `Agent::run_turn` streams `AgentEvent`s back (`AssistantTextDelta`,
   `ToolCallStarted`/`ToolCallResult`, `PhaseChanged`,
   `PermissionPromptNeeded`, `TokenUsage`, ...) — defined in `smith-core/src/event.rs`.
4. `App::on_agent_event` (`smith-tui/src/app.rs`) is the single place that
   turns those events into UI state (`self.lines`, `self.phase`,
   `self.activities`, modals, ...).

Adding a new capability that needs to reach the UI means: a new `AgentEvent`
variant in `event.rs`, an emitter in `agent.rs` (or `main.rs`), and a handler
arm in `app.rs::on_agent_event`.

### Tool interception pattern

`ask_user` is registered normally in `ToolRegistry` with a real
`ToolDefinition`/`permission_class`, but its `execute()` is a stub that
always errors — `Agent::run_one_tool` special-cases the tool by name *before*
the generic dispatch (permission checks, plan gate, `tools.execute()`) and
handles it separately, round-tripping through a `oneshot` channel
(`QuestionAsk`) out to the TUI and back. Use this pattern for any tool whose
result depends on interactive UI state rather than pure computation — don't
try to make `ToolExecutor::execute` itself interactive.

### Permission model

Every tool has a `PermissionClass`: `ReadOnly` (never prompts), `Mutating`
(prompts unless session-allowed), `Dangerous` (always prompts unless
session-allowed — `run_bash` and all MCP-bridged tools default here).
`PermissionPolicy` (`Ask`/`Session`/`Skip`, set via `/permission`) controls
which classes auto-allow on top of that.

Independently, `plan_gated` (set by `/plan <task>`, cleared by
`/plan approve`/`/plan reject`) blocks every tool above `ReadOnly` outright,
regardless of `PermissionPolicy` — even `Skip` doesn't bypass an unapproved
plan. Both checks happen in `Agent::run_one_tool`
(`crates/smith-core/src/agent.rs`).

### Session/goal persistence

Conversations persist per-project to `.smith/sessions.db` (SQLite, via
`smith-store::SessionStore`). `/goal` is stored as a column on the session
row and folded into every request's system prompt via
`Agent::effective_system`.
Both live under the project's `.smith/` directory (gitignored), separate from
the global `~/.smith/config.toml`.

### Headless mode

`smith -p "task"` runs one turn without a terminal. `--output-format` is
`text` (prose on stdout, all chrome on stderr, so `> out.txt` gets just the
reply), `json` (one object) or `stream-json` (one `AgentEvent` per line — the
same adjacently-tagged shape `AgentEvent` serializes to).

Exit codes: `0` success, `1` the turn failed, `2` usage/config error (matching
clap's own bad-flag code), `3` stopped by a safety cap. `3` is distinct from
`1` because they warrant opposite reactions in CI — a failure is a bug, a cap
is a budget to raise with all prior work intact.

Permissions deny by default; `--allowed-tools` is the only gate, and headless
forces `PermissionPolicy::Ask` regardless of saved config (a stored `skip`
would auto-allow tools before they ever reach the channel `--allowed-tools`
inspects). `ask_user` is refused rather than answered — there is no user, and
inventing one puts words in their mouth.

### Provider/model switching mid-session

`/model` (see `smith-cli/main.rs` `Action::SwitchModel` handling) can change
just the model or rebuild the `Agent` against a different provider entirely;
conversation history carries over either way, since `messages: Vec<Message>`
lives independently of the `Arc<dyn LlmProvider>`.
