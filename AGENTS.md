# AGENTS.md

See also `CLAUDE.md` (longer architecture narrative) and `README.md` (user-facing
slash commands). Rust stable is pinned via `rust-toolchain.toml`.

## Commands

```sh
cargo build --workspace
cargo run -p smith-cli                # run the TUI (dev)
cargo run -p smith-cli -- setup       # provider/model wizard

cargo test --workspace                # all tests
cargo test -p smith-tui               # one crate
cargo test -p smith-tui app::tests::plan_gate_changed_event_syncs_state  # one test
```

Tests are inline `#[cfg(test)] mod tests` at the bottom of each source file —
there are no separate test files.

CI (and pre-commit) gate, in this order — all must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Architecture (non-obvious)

7-crate workspace; dependencies flow one way toward `smith-core`, which is
pure traits/types (no HTTP, SQLite, or ratatui). `smith-tui` never talks to
`smith-providers`/`smith-tools` directly — only via `Action`/`AgentEvent`
channels.

The loop to understand before touching orchestration or TUI code:

1. TUI (`app.rs::on_key`) produces an `Action`.
2. `smith-cli/src/main.rs` matches on it (the `match action` loop) and drives
   `Agent::run_turn` (`smith-core/src/agent.rs` — the center of the system).
   Provider construction (`ProviderKind`, `build_provider`) and `Persistence`
   helpers live in `smith-cli/src/orchestrator.rs`.
3. `run_turn` streams `AgentEvent`s back (variants in `smith-core/src/event.rs`).
4. `App::on_agent_event` (`smith-tui/src/app.rs`) is the single place events
   become UI state.

Adding a capability that reaches the UI = new `AgentEvent` variant in
`event.rs` + emitter in `agent.rs`/`main.rs` + handler arm in
`app.rs::on_agent_event`.

### Tool interception pattern

`ask_user` is registered normally but its `execute()` is a stub that always
errors — `Agent::run_one_tool` special-cases the tool by name *before* generic
dispatch (permission checks, plan gate, `tools.execute()`) and round-trips
through a `oneshot` (`QuestionAsk`) channel to the TUI. Use this pattern for
any tool whose result depends on interactive UI state.

### Permissions and plan gate

Every tool has a `PermissionClass` (`ReadOnly` never prompts, `Mutating`
prompts unless session-allowed, `Dangerous` always prompts unless
session-allowed). `PermissionPolicy` (`Ask`/`Session`/`Skip`) layers on top.
Independently, a pending plan (`plan_gated`) blocks everything above
`ReadOnly` regardless of policy — even `Skip`. Both checks are in
`Agent::run_one_tool`. MCP-bridged tools default to `Dangerous`.

## Gotchas

- All TUI colors come from the Ember design tokens in
  `smith-tui/src/theme.rs` (truecolor with `COLORTERM`, ANSI fallback
  otherwise) — no `Color::` literal may appear outside that module. Visual
  primitives live in `smith-tui/src/components/`; the spec is
  `docs/design-system.md`.
- Mutating file tools (`write_file`, `edit_file`) stage content under
  `.smith/staging/<session_id>/…` before applying to the real path
  (`smith-tools/src/staging.rs`); tests assert no staging residue remains.
- `ToolRegistry::with_defaults` registers `web_search` with no API key; if a
  key is configured, `orchestrator.rs` re-registers it with the key
  (`register` overwrites by name — keep this order).
- Built-in tools: `read_file`, `list_dir`, `glob`, `write_file`, `edit_file`,
  `run_bash`, `ask_user`, `write_tasks`, `web_search`.
- Runtime state: global `~/.smith/config.toml`; per-project
  `.smith/sessions.db` and `.smith/goal.md` (gitignored, never commit).
- Env keys `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` take priority over saved
  config.
