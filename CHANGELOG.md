# Changelog

All notable user-facing changes are tracked here.

## Unreleased

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
