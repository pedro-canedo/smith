# cargo-binstall

`cargo-binstall` can install smith from GitHub release archives once the crate
metadata is published. The current release archives are named for the binary,
not the crate package:

```text
smith-<version>-<target>.tar.gz
smith-<version>-x86_64-pc-windows-msvc.zip
```

Before publishing to crates.io, add package metadata to `crates/smith-cli` and
verify it locally with:

```sh
cargo binstall --no-confirm smith-cli
```

Do not publish unverified metadata: a wrong `pkg-url` makes `cargo-binstall`
fall back to building from source, which defeats the clean-machine install
criterion for users without a Rust toolchain.
