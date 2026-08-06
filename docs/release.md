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

## Package-manager templates

- Shell installer: [`scripts/install.sh`](../scripts/install.sh)
- Homebrew formula template: [`packaging/homebrew/smith.rb`](../packaging/homebrew/smith.rb)
- Scoop manifest template: [`packaging/scoop/smith.json`](../packaging/scoop/smith.json)
- cargo-binstall notes: [`packaging/cargo-binstall.md`](../packaging/cargo-binstall.md)

Templates contain placeholder checksums until the corresponding GitHub release
exists. Do not publish a package-manager update with placeholder hashes.
