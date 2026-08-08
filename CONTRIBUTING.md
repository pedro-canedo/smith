# Contributing to smith

Thanks for looking. This is a small project with one maintainer, so the
short version is: open an issue before a large change, and keep the gates
green.

## The gates

Four commands. CI runs exactly these, so a green run here is a green run
there:

```sh
bash scripts/check-file-size.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Two notes that save a round trip:

- **Clippy is a gate, not advice.** `-D warnings` is also in
  `[workspace.lints]`, so a bare `cargo clippy` is already as strict as CI.
  CI may run a newer toolchain than yours and see a lint you do not; if a
  run fails on something your machine passed, `rustup update stable` first.
- **`check-file-size.sh` caps files at 1500 lines.** The fix is a real
  module boundary, not shredding a function. An exception is a line in
  `scripts/file-size-allowlist.txt`, reviewed in the diff.

## Tests

Tests live in a `#[cfg(test)] mod tests` belonging to the module they cover —
inline at the bottom of the file, or in a sibling `<module>/tests.rs` once
that block passes roughly 400 lines. Both are child modules: `use super::*`
reaches private items either way.

`crates/smith-cli/tests/` is for the two things an in-process test cannot
see — what the *process* does with pipes and exit codes, and what a panic
does to a real tty. Adding a file there to shorten another one means
publishing what is under test; don't.

Test names are sentences describing the guarantee, not labels:
`a_bad_request_is_not_retried`, not `test_retry`. Where a test exists because
something broke, say so in a comment above it — the bug is the reason the
test is allowed to be strange.

## Architecture, briefly

Eight crates, dependencies flowing one way toward `smith-core`, which knows
nothing about HTTP, SQLite or the terminal. There is deliberately no
`domain/`/`application/`/`infrastructure/` split: the crate graph already is
ports-and-adapters, and it is enforced by the compiler rather than by folder
names. `CLAUDE.md` is the long version and is worth reading before a change
that crosses crates.

Adding something that reaches the UI means: a new `AgentEvent` variant in
`smith-core/src/event.rs`, an emitter, and a handler arm in
`smith-tui`'s `on_agent_event` — that match is exhaustive with no wildcard,
on purpose. `AgentEvent`'s serialization **is** the `--output-format
stream-json` wire format, so renaming a variant or a field is a breaking
change even though nothing in the workspace reads it back.

## Pull requests

- One logical change per PR. If you find an unrelated bug on the way, say so
  in the description rather than fixing it in the same diff.
- Explain *why* in the commit message, not what — the diff already says what.
- Match the surrounding code's style, error handling and comment density. A
  comment should state a constraint the code cannot show; it should not
  narrate the next line.
- New behaviour needs a test. "Tested manually" is fine as extra evidence and
  not as the only evidence.
- Security-relevant findings go through [SECURITY.md](SECURITY.md), not a
  public PR.

## What is likely to be declined

- A dependency added without a paragraph on why the standard library or an
  existing dependency will not do.
- A second mechanism for something that already has one — a parallel config
  system, a second permission path, another way to register a tool.
- Reformatting, renaming or restructuring that is not in service of a change
  someone asked for.
