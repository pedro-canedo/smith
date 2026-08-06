<div align="center">

<pre>
███████╗███╗   ███╗██╗████████╗██╗  ██╗
██╔════╝████╗ ████║██║╚══██╔══╝██║  ██║
███████╗██╔████╔██║██║   ██║   ███████║
╚════██║██║╚██╔╝██║██║   ██║   ██╔══██║
███████║██║ ╚═╝ ██║██║   ██║   ██║  ██║
╚══════╝╚═╝     ╚═╝╚═╝   ╚═╝   ╚═╝  ╚═╝
</pre>

# Smith

### A terminal AI coding agent built in Rust

Read your codebase, make a plan, edit files, run commands, and keep you in
control of every consequential action — directly from your terminal.

<p>
  <a href="https://github.com/pedro-canedo/smith/actions/workflows/ci.yml"><img src="https://github.com/pedro-canedo/smith/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/pedro-canedo/smith/releases"><img src="https://img.shields.io/github/v/release/pedro-canedo/smith?display_name=tag&sort=semver" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/built_with-Rust-orange.svg" alt="Built with Rust"></a>
</p>

</div>

> **Early development.** Smith is useful today, but its interfaces are still
> evolving. Expect breaking changes while the project settles.

## Why Smith?

Smith is a local-first coding agent for people who want the leverage of an LLM
without giving up the terminal, the filesystem, or the final say.

- **Plan before changing code** — `/plan` creates an explicit plan and blocks
  mutating tools until you approve it.
- **Permission-aware tools** — read-only, mutating, and dangerous actions have
  different policies, with a prompt before anything consequential runs.
- **Undo file edits** — checkpoints let `/rewind` restore a turn's file changes;
  shell commands are clearly called out because they cannot be safely undone.
- **Interactive and scriptable** — use the full TUI, plain screen-reader output,
  or text/JSON/JSONL output in automation.
- **Bring your own model** — Anthropic, OpenAI, or a local Ollama server.
- **Extensible by design** — MCP servers, hooks, commands, skills, personas,
  and subagents are all project- or user-configurable.
- **Terminal-native** — streaming Markdown, session history, task tracking,
  model switching, ASCII mode, and no browser or desktop app required.

## Install

### Linux, macOS, and WSL2

```sh
curl -fsSL https://raw.githubusercontent.com/pedro-canedo/smith/main/scripts/install.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/pedro-canedo/smith/main/scripts/install.ps1 | iex
```

The installers download a published archive, verify its SHA-256 checksum, and
install Smith without Rust. The Unix installer uses `~/.local/bin` by default;
the PowerShell installer uses `%LOCALAPPDATA%\smith\bin` and updates the user
`PATH`.

### Updates

Interactive sessions check GitHub Releases at most once every 24 hours. The
check never blocks headless runs and never replaces the binary automatically:

```sh
smith update
```

`smith update` downloads the matching platform archive, verifies its SHA-256
checksum, and replaces the current executable. If you prefer automatic
updates, opt in explicitly with:

```sh
SMITH_AUTO_UPDATE=1 smith
```

Set `SMITH_DISABLE_UPDATE_CHECK=1` to disable the startup notice. Update
metadata is cached in `~/.smith/update-check.json`.

To install a specific version or directory:

```sh
curl -fsSL https://raw.githubusercontent.com/pedro-canedo/smith/main/scripts/install.sh \
  | SMITH_VERSION=v0.1.0 SMITH_INSTALL_DIR="$HOME/.local/bin" sh
```

See the [release checklist](docs/release.md) for artifact verification and
package-manager templates.

### From source

Requires [Rust stable](https://rustup.rs):

```sh
git clone https://github.com/pedro-canedo/smith
cd smith
cargo build --release --workspace
```

## Quick start

Configure a provider and model, then start Smith in a project:

```sh
smith setup
smith
```

The setup wizard supports Anthropic, OpenAI, and Ollama. Configuration is saved
to `~/.smith/config.toml`; environment variables take precedence for API keys:

```sh
export ANTHROPIC_API_KEY=...
smith --provider anthropic
```

Run one turn without opening the TUI:

```sh
smith --print "Explain the architecture of this project"
cat issue.md | smith --print "Implement this issue"
smith --output-format json --print "List the risky parts of this change"
```

For accessibility, CI, or narrow terminals:

```sh
smith --plain --print "Summarize the current project"
smith --ascii
TERM=dumb smith --print "Check the tests"
```

## How it works

```text
your prompt
    │
    ▼
Smith agent loop ── provider stream ── Anthropic / OpenAI / Ollama
    │
    ├── read-only tools run automatically
    ├── mutating and dangerous tools ask for permission
    ├── an approved plan can gate all changes
    └── events return to the TUI or headless output
```

Smith keeps project sessions in `.smith/sessions.db` and global settings in
`~/.smith/`. The project directory is the boundary for file tools; shell
commands are intentionally not sandboxed because a fake shell jail would give
you a false sense of safety.

## Everyday workflow

### Plan and execute

```text
/plan migrate the authentication layer to the new API
```

Review the proposed steps, then approve them. While a plan is pending, Smith
will not run mutating or dangerous tools — even if the permission policy would
otherwise allow them.

### Sessions and goals

```text
/goal Make the test suite deterministic
/continue
/usage
```

The goal is stored with the session, not in a project file. From the shell,
`smith sessions list`, `smith sessions export`, and `smith sessions fork` help
you inspect or branch conversation history.

### Undo file changes

```text
/rewind
/rewind confirm
```

Smith checkpoints files before `write_file` and `edit_file`. A rewind is
conservative: if a file changed after the original turn, Smith refuses to
overwrite it unless you explicitly use `--force`. It never claims to undo
`run_bash` or MCP side effects.

## Providers and configuration

The interactive wizard is the recommended starting point. For manual setup,
`~/.smith/config.toml` can contain:

```toml
[general]
provider = "anthropic"          # anthropic | openai | ollama
model = "your-model-name"
permission_policy = "ask"       # ask | session | skip

[anthropic]
api_key = "sk-ant-..."

[openai]
api_key = "sk-..."

[ollama]
base_url = "http://127.0.0.1:11434/v1"

[search]
# Optional: pin web_search to one backend.
backend = "searxng"
searxng_url = "https://searx.example.com"
market = "pt-BR"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allow"]
```

API keys from `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` override saved values.
Run `smith doctor` to check credentials, connectivity, directory permissions,
MCP servers, and optional runtimes without starting a session.

## Tools and safety

| Tool | Default class | Behavior |
| --- | --- | --- |
| `read_file`, `list_dir`, `glob`, `web_search`, `write_tasks`, `ask_user` | Read-only | Runs without a permission prompt |
| `write_file`, `edit_file` | Mutating | Prompts unless allowed for the session |
| `run_bash`, MCP tools | Dangerous | Requires explicit permission by default |

Use `/permission` to inspect or change the policy for the current session:

```text
/permission       # show the current policy
/permission ask
/permission session
/permission skip  # explicitly enables unrestricted tool approval
```

For non-interactive runs, allowed mutating tools must be named explicitly:

```sh
smith --print "Format the changed Rust files" \
  --allowed-tools write_file,edit_file,run_bash
```

Read the [authorization model](docs/authorization.md) for the interaction
between permissions, plans, hooks, and MCP tools.

## Customize Smith

Smith loads project extensions from `.smith/` and user extensions from
`~/.smith/`:

| Extension | Purpose | Documentation |
| --- | --- | --- |
| Commands | Reusable `/commands` with arguments | [extensions](docs/extensions.md) |
| Skills | On-demand instructions for recurring work | [extensions](docs/extensions.md) |
| Personas | Output styles selected with `--persona` | [extensions](docs/extensions.md) |
| Subagents | Specialized model roles | [extensions](docs/extensions.md) |
| Hooks | JSON-in/JSON-out policy and lifecycle hooks | [hooks](docs/hooks.md) |
| SearXNG | Self-hosted web search | [extensions](docs/extensions.md) |
| Themes | `[theme]` palette and per-token colours | [design system](docs/design-system.md) |
| Key bindings | `[keys]` remaps the panel shortcuts | [design system](docs/design-system.md) |

### Keys

| Key | Action |
| --- | --- |
| `Ctrl+B` | Show or hide the sidebar |
| `Shift+Tab` | Cycle the sidebar tab (Session / Tasks / Vitals) |
| `Ctrl+O` | Enter or leave tool-card focus |
| `Ctrl+L` | Diagnostics panel (also written to `~/.smith/logs/`) |
| `Ctrl+J` | Newline without submitting |
| `Up` / `Down` | Walk the prompt history, once past the edge of the input |
| `@` | Complete a file path from the project |
| Wheel / click | Scroll, and select a tool card |

The five bindings above the arrows are remappable, because the defaults
collide with common terminal setups — `Ctrl+B` is tmux's own prefix:

```toml
[keys]
toggle_sidebar = "ctrl+t"
```

Example command:

```text
.smith/commands/review.md
```

Then run it as:

```text
/review
```

## CLI reference

```text
smith [OPTIONS] [COMMAND]
```

| Option | Description |
| --- | --- |
| `--provider <NAME>` | Override the configured provider |
| `--model <NAME>` | Override the configured model |
| `--resume <ID>` / `--continue` | Resume a saved session |
| `-p, --print <PROMPT>` | Run one non-interactive turn |
| `--output-format <text\|json\|stream-json>` | Select headless output |
| `--plain` | Screen-reader-friendly linear output |
| `--ascii` | Force ASCII UI glyphs |
| `--theme <dark\|light\|high_contrast>` | Select the palette |
| `--cwd <DIR>` | Run against another project directory |
| `--allowed-tools <LIST>` | Allow named tools in headless mode |
| `--persona <NAME>` | Select an output style |
| `setup` | Configure provider, key, and model |
| `remember` | Add a standing instruction to `SMITH.md` |
| `sessions` | List, export, fork, or inspect sessions |
| `doctor` | Validate the local installation and configuration |
| `update` | Check for and install the latest published release |

Run `smith --help` or `smith <command> --help` for the complete reference.

## Architecture

Smith is an eight-crate Rust workspace with a one-way dependency flow toward
`smith-core`:

| Crate | Responsibility |
| --- | --- |
| `smith-core` | Agent loop, domain types, provider and tool traits |
| `smith-provider` | Anthropic, OpenAI, and Ollama adapters |
| `smith-tools` | Built-in tools and permissions registry |
| `smith-mcp` | JSON-RPC-over-stdio MCP client |
| `smith-store` | SQLite session persistence |
| `smith-config` | Layered config, memory, and extensions |
| `smith-tui` | `ratatui`/`crossterm` interface |
| `smith-cli` | CLI, orchestration, prompts, and headless mode |

The TUI communicates with the agent through `Action` and `AgentEvent` channels;
it never talks directly to providers or tools. See [CLAUDE.md](CLAUDE.md) for
the long-form architecture and [AGENTS.md](AGENTS.md) for contributor rules.

## Development

```sh
git clone https://github.com/pedro-canedo/smith
cd smith
cargo build --workspace
cargo run -p smith-cli
```

Before opening a pull request, run the same gates as CI:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests live inline in `#[cfg(test)] mod tests` blocks. Keep changes focused and
follow the architecture notes before adding a capability that crosses the TUI
and agent loop.

## Roadmap and status

Smith is currently focused on making the core loop, safety model, provider
adapters, terminal accessibility, and release workflow dependable. Planned
work includes broader provider coverage, richer diagnostics, and continued
hardening of native Windows and macOS behavior.

See [CHANGELOG.md](CHANGELOG.md) for shipped work and [docs/release.md](docs/release.md)
for the release process.

## Contributing

Bug reports, focused pull requests, and documentation improvements are
welcome. For security-sensitive issues, please avoid posting credentials or
reproduction data that contains secrets in a public issue.

## License

[MIT](LICENSE) © Pedro Canedo
