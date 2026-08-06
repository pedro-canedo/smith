# Release and packaging

The release workflow is intentionally GitHub Actions based for now. It already
builds the five published targets, uploads checksummed archives, and creates a
GitHub release. `cargo-dist` was evaluated for this wave, but replacing the
workflow would not remove a current failure mode: smith still needs custom
handling for Linux aarch64 C cross-linking, README/LICENSE staging, and package
manager templates. Revisit `cargo-dist` when signing, installers, or generated
package-manager metadata become the bottleneck.

## Release checklist

1. Update `CHANGELOG.md`.
2. Run the local gates:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

3. Confirm the platform matrix is green on GitHub Actions, especially Windows.
4. Tag the release:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

5. Verify the release assets:

   - `smith-<version>-x86_64-unknown-linux-gnu.tar.gz`
   - `smith-<version>-aarch64-unknown-linux-gnu.tar.gz`
   - `smith-<version>-x86_64-apple-darwin.tar.gz`
   - `smith-<version>-aarch64-apple-darwin.tar.gz`
   - `smith-<version>-x86_64-pc-windows-msvc.zip`
   - matching `.sha256` files

6. Install from one published artifact on a clean machine with no Rust
   toolchain and run:

   ```sh
   smith --version
   smith doctor
   ```

7. Verify both supported one-line installers from a clean machine:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/pedro-canedo/smith/main/scripts/install.sh | sh
   ```

   ```powershell
   irm https://raw.githubusercontent.com/pedro-canedo/smith/main/scripts/install.ps1 | iex
   ```

   The Unix installer targets Linux, macOS, and WSL2. The PowerShell
   installer targets native 64-bit Windows, installs under `%LOCALAPPDATA%`,
   updates the user `PATH`, and verifies the archive with `Get-FileHash`.

8. Verify an existing installation can update in place:

   ```sh
   smith update
   smith --version
   ```

   Startup notices are cached for 24 hours. Automatic replacement is opt-in
   with `SMITH_AUTO_UPDATE=1`; `SMITH_DISABLE_UPDATE_CHECK=1` disables the
   notice entirely.

## Package-manager templates

- Shell installer: [`scripts/install.sh`](../scripts/install.sh)
- Homebrew formula template: [`packaging/homebrew/smith.rb`](../packaging/homebrew/smith.rb)
- Scoop manifest template: [`packaging/scoop/smith.json`](../packaging/scoop/smith.json)
- cargo-binstall notes: [`packaging/cargo-binstall.md`](../packaging/cargo-binstall.md)

Templates contain placeholder checksums until the corresponding GitHub release
exists. Do not publish a package-manager update with placeholder hashes.

## The ten acceptance criteria

Where each one is actually checked. "Mechanism" and "behaviour" are separated
deliberately: several criteria are half a property of this code and half a
property of a model's judgement, and only the first half can be a `cargo test`.

| # | Criterion | Checked by |
| --- | --- | --- |
| 1 | Esc kills the child, session stays usable | `smith-core` cancel-safe tool loop tests + `smith-tools` process-group kill test |
| 2 | Resize during streaming | `resizing_mid_stream_never_leaves_a_row_wider_than_the_pane` |
| 3 | 200 messages compact, todos preserved | the compactor's pure carry-over function + the trigger test |
| 4 | `--resume` restores transcript, todos, cost | the `turns` ledger round trip; the TUI no longer recomputes cost at all |
| 5 | Ambiguous `edit_file` reports occurrences | mechanism: the error carries the count and an action. **Behaviour** (the model self-corrects) is not checked anywhere |
| 6 | Injection in a file is reported | mechanism: `injection::scan`, the fence, and the end-to-end read. **Behaviour** (the model does not obey) is not checked anywhere |
| 7 | 80x24, 16 colours, no Unicode | `every_row_fits_the_pane_at_80x24_in_ascii`, plus a per-preset WCAG AA sweep |
| 8 | `NO_COLOR=1 … json \| jq` | the `acceptance` CI job, in the criterion's literal form |
| 9 | A panic leaves the terminal clean | `tests/pty.rs`, under a real pseudo-terminal |
| 10 | Idle CPU ≈ 0% | `an_idle_smith_does_no_work` — asserted on the predicate the event loop wakes on, which is where it is decided |

### The gap, stated plainly

Criteria 5 and 6 have their mechanisms tested and their behaviour untested.
That is a deliberate split, not an oversight — "the model self-corrects" and
"the model does not obey" are properties of a model's judgement, they vary
between models and between versions of one model, and asserting them in
`cargo test` would produce a suite that goes red when a provider ships an
update. They belong in an eval suite run against real providers on a schedule,
reported as a rate rather than a pass/fail. **No such suite exists yet**, and
until it does nobody should read the table above as saying those two criteria
are covered end to end.

## Deliberately not adopted

Two items from the original roadmap were dropped after looking at what they
would replace. Recorded here so the next person does not re-derive it.

### `cargo-dist`

It would **replace** `.github/workflows/release.yml`, not extend it. That
workflow already does the five-target matrix, the archive layout, the SHA-256
sidecars and the GitHub release upload, and it does them in terms this
repository controls. Swapping it for a generator means the release process is
described by a tool's defaults rather than by a file in the repo, and the first
time a default disagrees with us the fix is a version pin instead of an edit.

Reconsider if the target matrix grows past what a hand-written matrix stays
readable at, or when Windows code signing lands — that is the point where the
generated workflow starts earning its keep.

### `xtask`

An `xtask` binary exists to give a repository a task runner. This one needs
three commands, they are the three in `CLAUDE.md`, and every contributor
already has `cargo`:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Adding a crate to alias them would add a compile step to the gate it is
aliasing. Reconsider when a release step needs real logic — generating
completions, stamping a build, assembling an installer — because that is
work a shell one-liner stops being able to hold.

## What has and has not actually run

Being explicit, because a release checklist that implies more verification
than happened is worse than a short one.

- **Verified on Linux:** the release profile builds; the release binary runs,
  reports its version, and passes `smith doctor`; `--panic-now` is absent from
  a release build, so the PTY test apparatus does not ship.
- **Verified since:** the release workflow has now run green on all five
  targets plus the publish step (tags `v0.1.0` and `v0.1.01`). The note above
  said it never had; it has.
- **Still not verified:** nobody has *run* the macOS or Windows artifacts. The
  release job proves they build and upload, not that they work.
- **The crate version is not bumped with the tag.** Both releases ship a binary
  that reports `smith 0.1.0`, so `smith update` sees `v0.1.01` as newer than
  the version it is already running and will keep offering the same update
  forever. Bump `[workspace.package] version` in the same commit as the tag.
- **Known risk on macOS:** `chrome-headless-shell` is downloaded unsigned and
  unnotarised, so Gatekeeper is expected to refuse it on first run. Nothing in
  `smith setup` handles that yet.
