# Extending smith

This page documents the user-owned extension points. Project-local files live
under `.smith/`; global files live under `~/.smith/`.

## Subagents

Subagents are global markdown files in `~/.smith/agents/*.md`. They define a
specialized child agent that the main agent can call through the `task` tool.

```md
---
name: doc-auditor
description: Checks docs against the current source tree.
tools: read_file, glob, grep
---

Read the requested docs and verify every factual claim against source files.
Report only mismatches and missing coverage.
```

Rules:

- Only global subagents are loaded. A repository cannot ship a subagent that
  silently runs on your API key.
- Markdown files are sorted by filename. If two define the same `name`, the
  first wins and the duplicate is reported at startup.
- A broken subagent file disables only that subagent; smith still starts.

## Custom slash commands

Custom commands are markdown prompt templates in `.smith/commands/**/*.md` and
`~/.smith/commands/**/*.md`.

Path names become command names:

- `.smith/commands/review.md` becomes `/review`
- `.smith/commands/db/migrate.md` becomes `/db:migrate`

Example:

```md
---
description: Review one module for unsafe assumptions.
---

Review $1 for error handling, permissions, and platform-specific assumptions.
Return concrete findings with file paths.
```

Placeholders:

- `$1` through `$9` are positional whitespace-separated arguments.
- `$ARGUMENTS` is the whole argument string.
- `$$` emits a literal dollar sign.

Custom commands cannot shadow built-ins such as `/clear` or `/plan`. Project
commands override global commands of the same name; shadowing is reported.

## Skills

Skills are on-demand instruction packs in `.smith/skills/<name>/SKILL.md` and
`~/.smith/skills/<name>/SKILL.md`.

```md
---
name: release-check
description: Use when preparing a release checklist and packaging audit.
---

Check CI, changelog, installation docs, artifact names, and platform caveats.
Prefer existing release automation over replacing it without a concrete gain.
```

Rules:

- The directory name is the skill name. Use letters, digits, `-`, and `_`.
- `description` is required because it is the only always-loaded part.
- The body is loaded only when the model calls the `skill` tool.
- Project skills override global skills of the same name.

## Personas

Personas are output styles in `.smith/personas/<name>.md` and
`~/.smith/personas/<name>.md`. Select one with `--persona <name>`.

```md
---
description: terse reviewer
mode: augment
---

Be concise. Lead with findings. Avoid motivational language.
```

`mode = "augment"` appends style guidance to the built-in prompt. `mode =
"replace"` replaces only the style half of the built-in prompt; safety and
truthfulness invariants remain in force.

If `default.md` exists, it is loaded when `--persona` is omitted. Use
`--persona none` to disable it.

## Hooks

Hooks are configured in `~/.smith/config.toml` and exchange JSON with smith.
See [`docs/hooks.md`](hooks.md) for the exact event schema and stdin/stdout
contract.

## SearXNG for `web_search`

SearXNG is the preferred free search backend when you run your own instance.
Configure it in `~/.smith/config.toml`:

```toml
[search]
searxng_url = "https://searx.example.com"
# Optional: prevent fallback to other engines.
backend = "searxng"
```

SearXNG disables JSON output by default. In the instance `settings.yml`, enable:

```yaml
search:
  formats:
    - html
    - json
```

Restart the instance after changing settings. Smith queries
`/search?format=json`; without JSON enabled, SearXNG returns HTTP 403.
