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

### Turn checkpoints and `/rewind`

Before any tool above `ReadOnly` is dispatched, `Agent::run_one_tool` asks the
`ToolExecutor` which paths that call is about to write
(`Tool::snapshot_paths`, defaulting to empty) and snapshots them through the
`smith_core::checkpoint::Checkpointer` trait. One central hook rather than one
per tool: it is the same choke point the plan gate and permission prompt use,
so a refused call never leaves an object behind, and a future mutating tool is
covered the day it declares its paths.

The implementation is `smith_tools::CheckpointStore` — content-addressed blobs
in `.smith/checkpoints/objects/`, one JSON manifest per turn in
`.smith/checkpoints/turns/<session>/<seq>.json`. Deliberately no git and
deliberately not in `sessions.db`: the store has to work with no session row at
all (headless), and a manifest that lives beside the objects it indexes cannot
disagree with them.

Two rules that are load-bearing, not incidental:

- **Checkpointing never fails a turn.** Every `Checkpointer` call is
  best-effort; a failure becomes an advisory `ToolProgress` line and the tool
  runs anyway.
- **`run_bash` cannot be snapshotted.** Any Mutating/Dangerous tool that
  declares no paths — `run_bash`, every MCP tool — is recorded as *uncovered*,
  and `/rewind` says so in the report rather than implying the turn was fully
  undone.

`/rewind` restores files only; it never truncates the conversation. Instead the
orchestrator queues `Agent::note_to_model`, which rides the *next* user message
(not a message of its own — that would leave two user messages in a row).

Don't confuse `smith-tools/src/checkpoint.rs` with `staging.rs`: staging holds
the *new* bytes of a write for the moment before it is applied and deletes
itself immediately; checkpoints hold the *old* bytes and persist.

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

### Web search backends

`web_search` (`smith-tools/src/web_search.rs`) tries three backends in order,
each falling through to the next on error or an empty page:

1. **Exa** — paid, structured, the only tier that reports publication dates.
   Needs `[exa] api_key` in `~/.smith/config.toml`.
2. **Headless Chromium** (`smith-tools/src/chromium.rs`) — launches whatever
   Chrome/Chromium-family browser is on `PATH` with `--headless --dump-dom`
   against DuckDuckGo's HTML endpoint and scrapes the rendered DOM. Free, and
   the reason `web_search` works with no key at all. `SMITH_CHROMIUM_PATH`
   (or `CHROME_PATH`) overrides binary discovery. Every launch gets a
   throwaway `--user-data-dir` so it never touches the user's real profile.
3. **DuckDuckGo lite over plain HTTP** — no browser needed, weakest results.

The Chromium tier is best effort by design: no browser, a failed launch, or
markup that changed shape all return `Err` and hand off to tier 3. Its parsing
is a class-name scanner rather than an HTML parser, so drift costs results,
never correctness. Only the pure halves are unit-tested; one `#[ignore]`d test
(`chromium::tests::live_search_returns_three_usable_results`) exercises the
real browser and network.

### The JSON action envelope

Models with no usable structured tool channel (small local ones especially)
can call a tool by replying with nothing but
`{"action": "web_search", "query": "..."}`. `Agent::recover_text_tool_call`
(`smith-core/src/agent.rs`) rebuilds any such object into a real `ToolUse`
before the turn ends, so it flows through the ordinary permission/execution
path. Two envelopes are accepted — that flat one, where the remaining
top-level fields *are* the arguments, and the nested
`{"name": ..., "arguments": {...}}` form. Both only fire when the name
resolves to exactly one registered tool: an `action` field is common enough in
ordinary JSON that dispatching on it blindly would turn quoted data into tool
calls.

Resolution (`resolve_tool_name`) is tolerant but never speculative. It accepts
an exact name, a name equal after normalisation (`Web-Search`, `WEB_SEARCH`,
`webSearch` → `web_search`), or a whole-segment prefix/suffix of at least four
characters (`search` → `web_search`) — and only when **one** registered tool
matches. `write` against both `write_file` and `write_tasks` resolves to
nothing rather than to a guess, and edit-distance matching is deliberately
absent. Arguments get the same treatment from `align_arguments`, which renames
an invented key onto the schema's own property when that is unambiguous
(`max_results` → `num_results`); keys it can't place are passed through for the
tool to reject, never dropped here.

### Reasoning tags in the text channel

Reasoning models emit `<think>`/`<thinking>`/`<reasoning>` blocks in the
*text* channel whenever the provider has no separate reasoning stream.
`ReasoningFilter` (`smith-core/src/agent.rs`) strips them inside
`consume_stream` — one shared place, because the leak follows the model, not
the wire format — before the deltas are forwarded *or* accumulated, so nothing
reaches the transcript, history, or the next request. It works on streamed
deltas (a tag may straddle two), leaves anything inside a ``` fence or
immediately after a backtick alone, and removes a stray closing tag without
eating the text around it. The reasoning itself is discarded; `Agent::
reasoning_tags_stripped()` counts what was removed.
