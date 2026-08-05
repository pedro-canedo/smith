# smith

```
███████╗███╗   ███╗██╗████████╗██╗  ██╗
██╔════╝████╗ ████║██║╚══██╔══╝██║  ██║
███████╗██╔████╔██║██║   ██║   ███████║
╚════██║██║╚██╔╝██║██║   ██║   ██╔══██║
███████║██║ ╚═╝ ██║██║   ██║   ██║  ██║
╚══════╝╚═╝     ╚═╝╚═╝   ╚═╝   ╚═╝  ╚═╝
```

A terminal-based AI coding agent, in the spirit of Claude Code: a fast, richly
styled TUI where you chat with an LLM and it can read/write files, run shell
commands, and call out to MCP servers to get things done — all built in Rust
for performance and terminal-rendering control.

**Status: early development.** The core chat loop (streaming, tool use,
multi-provider support) works; persistence, MCP, and polish are still to come.
See [Roadmap](#roadmap) below.

## Requirements

- Rust (stable toolchain — see `rust-toolchain.toml`), install via [rustup](https://rustup.rs)
- A configured provider — see [Provider setup](#provider-setup) below

## Building

```sh
cargo build --workspace
```

## Provider setup

The easiest way to configure a provider is the interactive wizard:

```sh
cargo run -p smith-cli -- setup
```

It walks you through: picking a provider (Anthropic, OpenAI, or a local model
via Ollama), entering an API key (for Anthropic/OpenAI) or picking a model
from a list of popular ones — or typing any model name yourself — and, for
Ollama, making sure the `ollama serve` daemon is running and pulling the model
for you. The result is saved to `~/.smith/config.toml` (permissions locked to
your user only). Run `smith setup model` later to jump straight to picking a
different model for the provider you already configured.

You can skip the wizard and configure things by hand instead:

- Anthropic/OpenAI: export `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` — these take
  priority over whatever is saved in the config file.
- Ollama: install it from [ollama.com](https://ollama.com/download), run
  `ollama serve`, and either run the wizard or hand-edit
  `~/.smith/config.toml`.

## Running (development)

```sh
cargo run -p smith-cli
```

By default `smith` uses whatever provider/model you saved with `smith setup`
(falling back to Anthropic's `claude-sonnet-5` if nothing is configured).
Override either per-run with flags:

```sh
cargo run -p smith-cli -- --provider ollama --model qwen2.5
```

Inside the TUI: type a message and press `Enter` to send it, `↑`/`↓`/`PageUp`/`PageDown`
scroll the transcript (it snaps back to following the latest message once you
scroll to the bottom), `Esc` cancels an in-flight response, and `Ctrl+C` quits.
`/clear` clears the visible transcript, `/help` lists commands, `/model` shows
or switches the active model/provider without restarting (see below), and
`/goal` sets a persistent session objective. Your messages
show in a bordered bubble; the model's replies flow freely (rendered live as
markdown while streaming — code blocks and inline code are styled). While the
agent is thinking or running tools, an animated widget shows a short summary
of each step (e.g. "Reading src/main.rs", "Running `cargo test`"). When the
agent wants to write a file or run a shell command, a permission modal asks
`[y]` allow once, `[a]` allow for the rest of the session, or `[n]` deny.

The sidebar (shown on wide enough terminals) tracks token usage for the
session, plus either: live CPU/RAM/VRAM/GPU stats when running a local Ollama
model (VRAM via `nvidia-smi` if present, falling back to Ollama's own
`/api/ps`), or a rough cost estimate (`~$0.0123 (est.)`) for token-billed
providers we have pricing for.

### `/model` — switch model or provider mid-session

```
/model                          show the current provider/model + known models
/model claude-haiku-4-5         switch model, same provider
/model ollama/qwen2.5           switch provider and model
/model gpt-4.1 --save           switch and persist as the new default
```

Conversation history carries over to the new model. Switching provider
resolves the API key the same way startup does (env var, then
`~/.smith/config.toml`) — if that provider isn't configured yet, the switch
fails with an error and the old model keeps running.

### `/permission` — control how often tools ask for confirmation

```
/permission                show the current mode
/permission ask            always prompt (default)
/permission session        auto-allow file writes/edits; still prompts for shell/MCP tools
/permission skip           auto-allow everything, including shell commands — no prompts at all
/permission skip --save    same, persisted as the default for future sessions
```

`skip` (alias `yolo`) removes the one safety net between the model and your
shell — smith prints an explicit warning the moment you enable it. Per-tool
"allow for this session" grants from the confirmation modal still work
independently of whichever mode you're in.

### `/usage` — session token/cost/tool-call summary

```
/usage
```

Prints requests sent, tools invoked, input/output token counts, and either an
estimated cost (token-billed providers we have pricing for) or `n/a` if we
don't — same honesty rule as the sidebar's cost estimate: no pricing data
means no number, not a guess.

### `/plan` — propose, then confirm & build

```
/plan <task description>   ask the model for a plan (steps, risks, affected files); no mutations yet
/plan                       show whether a plan is pending approval
/plan approve               approve & start building (same as [y] in the plan modal)
/plan reject                discard the plan (same as [n] / Esc)
```

While planning or awaiting approval, the input chrome switches to **plan mode**
(magenta border / `PLAN MODE` in the sidebar). When the plan turn finishes, a
**plan ready** modal shows the plan — `[y]`/`Enter` builds it, `[n]`/`Esc`
rejects. Type `/` for slash-command hints; `Tab` autocompletes.

While a plan is pending, **every** Mutating or Dangerous tool call is blocked
outright — including under `/permission skip` — until you approve or reject
it. Read-only investigation (`read_file`, `glob`, ...) is still allowed during
planning so the model can look before it proposes.

### `/goal` — persistent session objective

```
/goal                          show the current goal (if any)
/goal <description>            set the session goal
/goal clear                    remove it
```

The goal is written to `.smith/goal.md` in the project directory, so it
survives restarts. On every request it's folded into the system prompt so the
model keeps working toward it unless you direct otherwise.

Conversations are saved per-project to `.smith/sessions.db`. If a project has
prior history, the idle screen shows a "Continue" hint — resume it with:

```sh
smith --resume <session-id>
```

## MCP servers

Add stdio-transport MCP servers to `~/.smith/config.toml`:

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allow"]
```

Their tools are pulled in automatically at startup and, like `run_bash`,
always require a permission prompt — an arbitrary server's tool semantics
can't be assumed safe.

## Project layout

A Cargo workspace, split so the network/DB-heavy layers stay out of pure logic
crates and each layer has an independent test surface:

| Crate               | Responsibility                                                                                                                                    |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `smith-core`      | Domain types (`Message`, `ContentBlock`, `StreamEvent`), the `LlmProvider`/`Tool` traits, and the agent orchestration loop              |
| `smith-providers` | LLM provider adapters (Anthropic, OpenAI, and Ollama via the OpenAI-compatible adapter) implementing`LlmProvider`, including SSE stream parsing |
| `smith-tools`     | Built-in tools (`read_file`, `write_file`, `edit_file`, `list_dir`, `glob`) and the `ToolRegistry`                                    |
| `smith-mcp`       | MCP client (hand-rolled JSON-RPC over stdio) — bridges remote MCP server tools into the same`Tool` trait as built-ins                          |
| `smith-persist`   | Global config (`~/.smith/config.toml`) loading/saving, and per-project session history (`.smith/sessions.db`, SQLite)                            |
| `smith-tui`       | The`ratatui`/`crossterm` terminal UI — chat pane, input box, permission modal                                                                |
| `smith-cli`       | Binary entry point: CLI flags, wires every crate together                                                                                         |

Dependencies flow one way: `smith-core` defines the traits and knows nothing
about HTTP, SQLite, or `ratatui`; every other crate implements against those
traits. `smith-tui` never talks to `smith-providers` directly — only through
the `Action`/`AgentEvent` channels.

## Development workflow

Before committing, all three must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Roadmap

- [X] M0 — Workspace skeleton
- [X] M1 — Static TUI shell (banner, input box, panic-safe terminal restore)
- [X] M2 — Anthropic chat
- [X] M3 — Streaming responses + Esc-to-cancel
- [X] M4 — Provider abstraction + OpenAI
- [X] M5 — Built-in file tools + permission modal
- [X] M6 — Shell tool (`run_bash`) + full allow-once/allow-session/deny model
- [X] M7 — SQLite session persistence (per-project `.smith/sessions.db`, `--resume <id>`, idle-screen "Continue session" hint)
- [X] M8 — MCP client (stdio transport, tools bridged with default `Dangerous` permission)
- [X] M9 — Polish: markdown code-block/inline-code rendering, scrollback with auto-follow, `/clear` and `/help` slash commands

Also done ahead of schedule: a `smith setup` wizard for configuring providers/models (including pulling and running local Ollama models); a visual rework of the TUI (idle screen, chat sidebar, footer status bar) inspired by OpenCode's layout; user messages in a bubble with the model's replies flowing freely, live markdown rendering during streaming (via `tui-markdown`, including tables/headings/emphasis), wrap-aware auto-scroll, an animated per-step activity widget for tool calls; and a sidebar with live tok/s plus a resource panel (CPU/RAM/VRAM/GPU for local Ollama models, an estimated cost for token-billed providers).

### Slash commands

Incremental delivery, one command per package:

- [X] `/model` — runtime provider/model switching (see above)
- [X] `/permission` — runtime tool permission policy (see above)
- [X] `/usage` — session token/cost/tool-call summary
- [X] `/plan` — planning mode with a confirm-before-executing gate
- [X] `/goal` — persistent session objective (`.smith/goal.md`)
- [ ] `/loop` — run a prompt/goal repeatedly with a stop condition
- [ ] `/kanban` — simple todo/doing/done board tied to `/goal` and `/plan`
- [ ] `/ultraplan` — deep multi-step planning producing a persistent artifact

Known gaps worth knowing about if you're picking this up: MCP server tools use a fixed 30s call timeout with no cancellation of the underlying server process on repeated timeouts. Markdown is rendered via `tui-markdown` (headings, emphasis, tables, code); LaTeX math stays as literal delimiters in the terminal.

## Contributing

Issues and PRs are welcome. Please run the development workflow checks above
before opening a PR, and keep changes scoped — this is an early-stage project
and architectural changes are easier to review in small pieces.

## License

[MIT](LICENSE)
