# Changelog

All notable user-facing changes are tracked here.

## Unreleased

### Added

- Accessibility release work:
  - `--ascii` forces ASCII-only TUI glyphs without mutating `SMITH_ASCII`.
  - `--plain` uses the existing headless frontend for linear, screen-reader
    friendly output with no colour.
  - `TERM=dumb` automatically avoids the TUI path.
  - The TUI now restores terminal state on ordinary error exits as well as
    panic unwinds.
  - A PTY regression test verifies induced panics leave alternate screen and
    raw mode behind.
- User extension documentation for subagents, custom slash commands, skills,
  personas, hooks, and SearXNG.
- Release/packaging documentation plus install/Homebrew/Scoop templates.

### Changed

- The 80x24 ASCII render test now asserts the rendered buffer is entirely
  ASCII, matching the release acceptance criterion directly.

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
