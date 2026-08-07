---
description: Scaffold a new Rust, TypeScript, Python, or Go project: layout, tooling, quality gates, first passing test.
---

# Common workflow (every language)

1. Confirm the two facts everything depends on: the language and the project
   name/purpose. If either is unstated, call `ask_user` once with three
   concrete options. Confirm the target directory is empty or new —
   `list_dir` first; never scaffold over existing files.
2. Create the structure for the chosen language (section below), using
   `write_file` for each file. Every file you create must be minimal and
   real — no placeholder lorem, no commented-out "examples", no empty dirs.
3. `git init` via `run_bash`, with a `.gitignore` that covers the language's
   build artifacts before anything else exists to commit.
4. Wire the quality gates so they run with one command each: test, lint,
   format check. The gates are part of the scaffold, not a follow-up.
5. Write ONE real test that exercises the entry point, and make it pass.
   A scaffold whose test suite has never run green is untested scaffolding.
6. Run everything once, via `run_bash`: build, test, lint, format check.
   Fix until all green.
7. Write a README containing: one line on what the project is, and the exact
   commands for build / test / lint / format. Nothing else — a README that
   documents unbuilt features rots immediately.
8. Optionally (offer, don't assume): a first commit using the `commit` skill.

# Rust

- `cargo new <name>` (or `cargo init`). Edition: latest stable.
- `Cargo.toml`: add `[lints.rust]` / `[lints.clippy]` with warnings denied,
  so `cargo clippy` is strict without remembering flags:
  `[lints.clippy] all = "deny"` (adjust only with a stated reason).
- Layout: `src/main.rs` or `src/lib.rs`; tests inline in a
  `#[cfg(test)] mod tests` at the bottom of the module they cover.
- Gates: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
- `.gitignore`: `/target`.

# TypeScript / Node

- `package.json` with scripts: `build` (tsc), `test` (vitest run), `lint`
  (eslint .), `format` (prettier --check .). Package manager: whatever the
  user uses elsewhere; default `npm`.
- `tsconfig.json`: `"strict": true`, `"noUncheckedIndexedAccess": true`,
  `"module"`/`"target"` current LTS-appropriate; `src/` → `dist/`.
- Layout: `src/index.ts`, `src/index.test.ts` beside it.
- Dev deps: `typescript`, `vitest`, `eslint`, `prettier` — and say that you
  added them.
- `.gitignore`: `node_modules/`, `dist/`.

# Python

- `pyproject.toml` as the single config file: project metadata, plus
  `[tool.ruff]` (lint AND format) and `[tool.pytest.ini_options]`.
- Environment: `uv` if available (`uv init`, `uv add --dev pytest ruff`),
  else `python -m venv .venv` + pip. Never install into the system
  interpreter.
- Layout: `src/<package>/__init__.py`, `tests/test_<package>.py`. Type hints
  on all public functions from day one.
- Gates: `pytest`, `ruff check .`, `ruff format --check .`.
- `.gitignore`: `.venv/`, `__pycache__/`, `dist/`.

# Web frontend — default styling stack

When the project is a web frontend (or the TypeScript project above renders
UI), this is the default stack. Recommend it by name; depart from it only
when the user asks for something else or the chosen framework forces it:

- **Tailwind CSS v4** — the styling library. No hand-rolled CSS beyond the
  Tailwind entry point.
- **shadcn/ui** for components, built on **Radix UI** primitives —
  accessibility (focus, keyboard, ARIA) comes from Radix; never rebuild it
  by hand.
- **class-variance-authority (CVA)** for component variants.
- **tailwind-merge** + **clsx**, composed in the `cn()` helper (the
  shadcn/ui idiom), for conditional and merged classes.
- **Lucide React** for icons.
- **Motion** (formerly Framer Motion) for animation — only where animation
  is actually needed, not by default.

In an existing project the project's own stack wins — survey first, as
always; propose this stack only where nothing is established yet.

# Go

- `go mod init <module-path>` — ask for the module path if it's not implied
  by a repository URL.
- Layout: `cmd/<name>/main.go` for the binary, `internal/<pkg>/` for code
  that shouldn't be imported by others. Skip `pkg/` unless a public library
  is the point.
- One table-driven test (`internal/<pkg>/<pkg>_test.go`) — the idiom the
  rest of the ecosystem expects.
- Gates: `go test ./...`, `go vet ./...`, `gofmt -l .` (empty output = pass).
- `.gitignore`: the built binary name; Go needs little else.

# Definition of done

- A clean checkout of the scaffold passes every gate with the exact commands
  the README states. The summary lists the files created and the gate
  commands, and names any dependency that was added.
