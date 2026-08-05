<div align="center">

```
███████╗███╗   ███╗██╗████████╗██╗  ██╗
██╔════╝████╗ ████║██║╚══██╔══╝██║  ██║
███████╗██╔████╔██║██║   ██║   ███████║
╚════██║██║╚██╔╝██║██║   ██║   ██╔══██║
███████║██║ ╚═╝ ██║██║   ██║   ██║  ██║
╚══════╝╚═╝     ╚═╝╚═╝   ╚═╝   ╚═╝  ╚═╝
```

**A terminal AI coding agent, written in Rust.**

Chat with an LLM in a fast, richly styled TUI while it reads and writes your
files, runs shell commands, searches the web, and calls MCP servers — with a
permission prompt before anything touches your machine.

[![CI](https://github.com/pedro-canedo/smith/actions/workflows/ci.yml/badge.svg)](https://github.com/pedro-canedo/smith/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

</div>

---

> **Status: early development.** The agent loop, tools, providers, persistence,
> and MCP all work day to day, but interfaces still change between commits.
> See the [Roadmap](#roadmap).

## Features

- **Bring your own model** — Anthropic, OpenAI, or anything local through
  [Ollama](https://ollama.com). Switch provider or model mid-conversation with
  `/model`; history carries over.
- **Real tools** — read, write, and edit files, list directories, glob, run
  shell commands, search the web, and keep a live task checklist.
- **Permission model you control** — every tool is classified read-only,
  mutating, or dangerous. Nothing above read-only runs without your `y`/`a`/`n`.
- **Plan before you build** — `/plan <task>` makes the agent propose steps,
  risks, and affected files. Until you approve, *every* mutating tool is
  blocked outright.
- **MCP support** — bridge any stdio-transport MCP server's tools in as
  first-class tools.
- **Persistent sessions** — conversations are saved per project; resume with
  `--resume`. `/goal` keeps a long-lived objective in the system prompt.
- **Built for the terminal** — live markdown rendering while streaming,
  wrap-aware scrollback, a per-step activity widget, and a sidebar with token
  usage plus live CPU/RAM/VRAM stats (local models) or a cost estimate
  (token-billed providers).

## Installation

### Prebuilt binaries

Grab the archive for your platform from the
[latest release](https://github.com/pedro-canedo/smith/releases/latest) and put
`smith` somewhere on your `PATH`.

<details>
<summary><strong>Linux</strong> (x86_64 / arm64)</summary>

```sh
# pick one: x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu
TARGET=x86_64-unknown-linux-gnu
VERSION=$(curl -fsSL https://api.github.com/repos/pedro-canedo/smith/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)

curl -fsSL "https://github.com/pedro-canedo/smith/releases/download/${VERSION}/smith-${VERSION#v}-${TARGET}.tar.gz" | tar xz
sudo install "smith-${VERSION#v}-${TARGET}/smith" /usr/local/bin/
```

</details>

<details>
<summary><strong>macOS</strong> (Apple Silicon / Intel)</summary>

```sh
# pick one: aarch64-apple-darwin (M1+) | x86_64-apple-darwin (Intel)
TARGET=aarch64-apple-darwin
VERSION=$(curl -fsSL https://api.github.com/repos/pedro-canedo/smith/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)

curl -fsSL "https://github.com/pedro-canedo/smith/releases/download/${VERSION}/smith-${VERSION#v}-${TARGET}.tar.gz" | tar xz
sudo install "smith-${VERSION#v}-${TARGET}/smith" /usr/local/bin/
```

The binaries are unsigned, so Gatekeeper will quarantine them on first run:

```sh
xattr -d com.apple.quarantine /usr/local/bin/smith
```

</details>

<details>
<summary><strong>Windows</strong> (x86_64)</summary>

```powershell
$version = (Invoke-RestMethod https://api.github.com/repos/pedro-canedo/smith/releases/latest).tag_name
$name    = "smith-$($version.TrimStart('v'))-x86_64-pc-windows-msvc"

Invoke-WebRequest "https://github.com/pedro-canedo/smith/releases/download/$version/$name.zip" -OutFile "$name.zip"
Expand-Archive "$name.zip" -DestinationPath .
# then move $name\smith.exe somewhere on your PATH
```

Use Windows Terminal — the legacy console host doesn't render the TUI correctly.

</details>

Each archive ships with a `.sha256` file next to it if you want to verify the
download.

### With cargo (any OS)

Requires a [stable Rust toolchain](https://rustup.rs):

```sh
cargo install --git https://github.com/pedro-canedo/smith smith-cli
```

### From source

```sh
git clone https://github.com/pedro-canedo/smith
cd smith
cargo build --release --workspace
# binary at ./target/release/smith
```

## Quick start

```sh
smith setup     # interactive provider + model wizard
smith           # start the TUI in the current project
```

The wizard walks you through picking a provider (Anthropic, OpenAI, or a local
model via Ollama), entering an API key or choosing a model, and — for Ollama —
making sure `ollama serve` is running and pulling the model for you. It saves
to `~/.smith/config.toml`, locked to your user.

Override per run without touching the config:

```sh
smith --provider ollama --model qwen2.5
smith --resume <session-id>
```

## Usage

### Keys

| Key | Action |
| --- | --- |
| `Enter` | send the message |
| `Esc` | cancel the in-flight response |
| `↑` `↓` `PageUp` `PageDown` | scroll the transcript (snaps back to follow-latest at the bottom) |
| `Tab` | autocomplete a slash command |
| `Ctrl+C` | quit |

When the agent wants to write a file or run a shell command, a modal asks:
`[y]` allow once, `[a]` allow for the rest of the session, `[n]` deny.

### Slash commands

| Command | What it does |
| --- | --- |
| `/help` | list available commands |
| `/model` | show or switch provider/model — `/model ollama/qwen2.5`, `--save` to persist |
| `/permission` | tool permission policy: `ask` (default), `session`, `skip` |
| `/plan <task>` | propose a plan first; `approve`/`reject` to unblock tools |
| `/goal <text>` | persistent session objective, stored in `.smith/goal.md` |
| `/loop [N] <task>` | repeat a task until done, N iterations, or `Esc` (`/loop goal` reuses the goal) |
| `/usage` | session requests, tool calls, tokens, and estimated cost |
| `/clear` | clear the visible transcript |

### Built-in tools

| Tool | Permission |
| --- | --- |
| `read_file`, `list_dir`, `glob`, `web_search`, `write_tasks`, `ask_user` | read-only — never prompts |
| `write_file`, `edit_file` | mutating — prompts unless session-allowed |
| `run_bash`, all MCP-bridged tools | dangerous — always prompts unless session-allowed |

`/permission skip` (alias `yolo`) auto-allows everything, including shell
commands — smith prints an explicit warning the moment you enable it. A pending
`/plan` blocks every mutating and dangerous tool regardless of policy, even
under `skip`.

## Configuration

Global config and secrets live in `~/.smith/config.toml`. Per-project state —
session history (`sessions.db`), the current goal, and staged edits — lives in
`.smith/` inside the project (add it to your `.gitignore`).

```toml
[general]
provider = "anthropic"          # anthropic | openai | ollama
model = "claude-sonnet-5"
permission_policy = "ask"       # ask | session | skip

[anthropic]
api_key = "sk-ant-..."

[openai]
api_key = "sk-..."

[ollama]
base_url = "http://127.0.0.1:11434/v1"

# Optional: primary web_search backend. Without a key, web_search falls back
# to Exa's keyless endpoint and then to DuckDuckGo Lite.
[exa]
api_key = "..."

# Any stdio-transport MCP server. Its tools are pulled in at startup and, like
# run_bash, always require a permission prompt.
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allow"]
```

`ANTHROPIC_API_KEY` and `OPENAI_API_KEY` take priority over whatever is saved
in the config file.

## Architecture

A Cargo workspace of 7 crates, with dependencies flowing one way toward
`smith-core` — which is pure traits and types and knows nothing about HTTP,
SQLite, or `ratatui`.

| Crate | Responsibility |
| --- | --- |
| `smith-core` | Domain types, the `LlmProvider`/`Tool` traits, and the agent loop (`agent.rs`) |
| `smith-provider` | Provider adapters (Anthropic, OpenAI, Ollama) including SSE stream parsing |
| `smith-tools` | Built-in tools and the `ToolRegistry` |
| `smith-mcp` | Hand-rolled JSON-RPC-over-stdio MCP client |
| `smith-store` | Global config and per-project SQLite session history |
| `smith-tui` | The `ratatui`/`crossterm` UI — chat pane, input box, modals, sidebar |
| `smith-cli` | Binary entry point: CLI flags, system prompt, orchestrator loop |

The TUI never talks to providers or tools directly — only through `Action` and
`AgentEvent` channels. [`CLAUDE.md`](CLAUDE.md) has the long-form narrative;
[`AGENTS.md`](AGENTS.md) is the short version.

## Contributing

Issues and pull requests are welcome — especially bug reports with a
reproduction, and small, focused PRs.

**Setup**

```sh
git clone https://github.com/pedro-canedo/smith
cd smith
cargo build --workspace
cargo run -p smith-cli          # run the TUI in dev
```

**Before you open a PR**, all three must pass — CI runs exactly these:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**Conventions**

- Tests are inline `#[cfg(test)] mod tests { ... }` at the bottom of the file
  they cover. There are no separate test files.
- Run a single crate or test with `cargo test -p smith-tui` /
  `cargo test -p smith-tui app::tests::some_test`.
- Keep changes scoped. This is early-stage; architectural changes are much
  easier to review in small pieces — open an issue to discuss first.
- Adding a capability that reaches the UI means three edits: a new `AgentEvent`
  variant in `smith-core/src/event.rs`, an emitter in `agent.rs` or `main.rs`,
  and a handler arm in `smith-tui/src/app.rs::on_agent_event`.

## License

[MIT](LICENSE) © Pedro Canedo
