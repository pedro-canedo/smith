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
`smith-provider`/`smith-tools` directly — only via `Action`/`AgentEvent`
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
- File tools are confined to the session directory (`fs_tools.rs::resolve`):
  `..` is normalised lexically *before* the prefix check (`starts_with` is
  component-wise, so `<root>/a/../../etc/passwd` would otherwise pass) and
  symlinks are resolved by canonicalising. `run_bash` is deliberately NOT
  confined — jailing a shell needs a real sandbox.
- Mutating file tools stage content under `.smith/staging/<session_id>/…`
  before applying (`smith-tools/src/staging.rs`). This is a crash-safety
  mechanism, **not** a security boundary: it sanitises its own mirror and
  then copies to the unsanitised target. The path jail above is the boundary.
- Tool registration has three entry points by intent: `try_register` fails on
  a name collision (what untrusted MCP tools go through), `register` panics
  (trusted startup wiring), `replace` overwrites deliberately (only used to
  upgrade the keyless `web_search` once an Exa key is configured).
- MCP tools are exposed as `mcp__{server}__{tool}`; the wire call still uses
  the bare remote name. Without the prefix a remote server publishing
  `read_file` would displace the sandboxed built-in.
- `tool_defs()` is sorted by name. The tool array is part of the prefix
  providers cache on, and a `HashMap`'s order would miss the cache on every
  request with no visible symptom.
- Every `tool_use` must be answered by a `tool_result` on *every* exit path
  from the tool loop, cancellation included — `agent.rs` seeds the results
  vector and fills it in rather than appending, so the invariant can't
  regress. A dangling `tool_use` makes the next request fail outright.
- Known API keys are stripped from tool output at one choke point in
  `run_one_tool` (`smith-core/src/redact.rs`), which covers the transcript,
  the session database and the next provider request together.
- `AgentEvent`'s serialization is adjacently tagged (`{"type":…,"data":…}`)
  because that *is* the `--output-format stream-json` wire format. Variant
  and field names are a public interface. `app.rs::on_agent_event` matches it
  exhaustively with no wildcard, so a new variant is never purely additive to
  smith-core — it always needs a TUI arm too.
- `smith-core` has a `testkit` feature (`testkit.rs`) exposing
  `ScriptedProvider`: replays scripted `StreamEvent`s, records the
  `CompletionRequest`s it received, and can fail a given request. Reach for it
  before hand-rolling another `impl LlmProvider`. Running past the end of a
  script panics on purpose — a turn making more requests than scripted is a
  finding, not something to paper over with a default reply.
- Built-in tools: `read_file`, `list_dir`, `glob`, `write_file`, `edit_file`,
  `run_bash`, `ask_user`, `write_tasks`, `web_search`.
- Runtime state: global `~/.smith/config.toml`; per-project
  `.smith/sessions.db` (gitignored, never commit). `/goal` is a column on the
  session row, not a file — several doc comments still say `.smith/goal.md`.
- Env keys `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` take priority over saved
  config.
- MSRV is 1.88, set in `[workspace.package]` and enforced by a CI job. The
  floor comes from the dependency graph (ratatui 0.30, darling 0.24), not
  from first-party syntax.
