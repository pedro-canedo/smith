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
- **Not verified anywhere:** the release workflow itself has never run — no
  tag has been pushed. macOS and Windows have only ever been exercised by the
  CI test matrix, never by the release job.
- **Known risk on macOS:** `chrome-headless-shell` is downloaded unsigned and
  unnotarised, so Gatekeeper is expected to refuse it on first run. Nothing in
  `smith setup` handles that yet.
