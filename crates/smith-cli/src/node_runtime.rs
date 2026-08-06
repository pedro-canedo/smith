//! Provisions a portable Node.js and the 9Router gateway into
//! `~/.smith/runtime/`, on the same pattern `runtime.rs` uses for Chromium:
//! staged download, checksum, probe-before-rename, `smith doctor` remedies.
//!
//! Two deliberate differences from the Chromium path:
//!
//! - **Real checksums.** nodejs.org publishes `SHASUMS256.txt` per release, so
//!   the verification here is sha256 against a published manifest —
//!   `Integrity::Md5Verified`'s caveat (hash and bytes from the same origin
//!   over the same connection) still applies, but the algorithm is not a
//!   storage-layer convenience.
//! - **A pinned gateway version.** `9router@` is installed at an exact
//!   version, never `latest`: it is a gateway that proxies credentials, and
//!   auto-updating that on other people's machines would be the wrong
//!   default. Bumping the pin is a smith release.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use smith_config::Config;

use crate::runtime::{safe_entry_path, AssetSource};

/// Pinned Node LTS ("Krypton" line). Bump deliberately, with the SHASUMS the
/// release publishes.
pub const NODE_VERSION: &str = "24.19.0";
/// Pinned gateway version. See the module doc for why this is never `latest`.
pub const NINEROUTER_VERSION: &str = "0.5.50";

const NODE_DIST_BASE: &str = "https://nodejs.org/dist";
/// Default port the gateway listens on; `[9router] base_url` overrides.
const NINEROUTER_PORT: u16 = 20128;

/// The Node builds smith knows how to unpack, mirroring `CftPlatform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePlatform {
    LinuxX64,
    LinuxArm64,
    MacX64,
    MacArm64,
    WinX64,
}

impl NodePlatform {
    pub fn detect() -> Option<Self> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn for_target(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxX64),
            ("linux", "aarch64") => Some(Self::LinuxArm64),
            ("macos", "x86_64") => Some(Self::MacX64),
            ("macos", "aarch64") => Some(Self::MacArm64),
            ("windows", "x86_64") => Some(Self::WinX64),
            _ => None,
        }
    }

    /// The token nodejs.org uses in file names.
    fn dist_token(self) -> &'static str {
        match self {
            Self::LinuxX64 => "linux-x64",
            Self::LinuxArm64 => "linux-arm64",
            Self::MacX64 => "darwin-x64",
            Self::MacArm64 => "darwin-arm64",
            Self::WinX64 => "win-x64",
        }
    }

    /// Archive file name for a version, e.g. `node-v24.19.0-linux-x64.tar.gz`.
    pub fn archive_name(self, version: &str) -> String {
        let extension = if matches!(self, Self::WinX64) {
            "zip"
        } else {
            "tar.gz"
        };
        format!("node-v{version}-{}.{extension}", self.dist_token())
    }

    /// Directory at the root of the archive.
    pub fn archive_root(self, version: &str) -> String {
        format!("node-v{version}-{}", self.dist_token())
    }

    /// The node binary inside the unpacked tree.
    pub fn node_binary(self) -> &'static str {
        if matches!(self, Self::WinX64) {
            "node.exe"
        } else {
            "bin/node"
        }
    }

    /// npm's own CLI entry, invoked as `node npm-cli.js` — portable across
    /// platforms, no symlinks or `.cmd` shims involved.
    pub fn npm_cli(self) -> &'static str {
        if matches!(self, Self::WinX64) {
            "node_modules/npm/bin/npm-cli.js"
        } else {
            "lib/node_modules/npm/bin/npm-cli.js"
        }
    }
}

/// Finds `file`'s hash in a `SHASUMS256.txt` body (`<hex>  <name>` lines).
pub fn parse_shasums(text: &str, file: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == file && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// The outcome of a successful `provision_node`.
#[derive(Debug, Clone)]
pub struct ProvisionedNode {
    pub version: String,
    pub binary: PathBuf,
    pub reported_version: String,
    pub reused: bool,
}

/// Where a given version installs to, under the runtime root.
fn node_install_dir(root: &Path, version: &str, platform: NodePlatform) -> PathBuf {
    root.join("node")
        .join(format!("{version}-{}", platform.dist_token()))
}

/// Downloads, verifies (sha256 against `SHASUMS256.txt`), unpacks and probes
/// a portable Node. Reuses an existing install whose binary still answers.
pub async fn provision_node(
    source: &dyn AssetSource,
    root: &Path,
    out: &mut Vec<String>,
) -> Result<ProvisionedNode, String> {
    let platform = NodePlatform::detect()
        .ok_or_else(|| format!("unsupported platform for Node: {}", std::env::consts::ARCH))?;
    let install_dir = node_install_dir(root, NODE_VERSION, platform);
    let binary = install_dir
        .join(platform.archive_root(NODE_VERSION))
        .join(platform.node_binary());

    if binary.is_file() {
        if let Ok(reported) = crate::runtime::probe_version(&binary).await {
            return Ok(ProvisionedNode {
                version: NODE_VERSION.to_string(),
                binary,
                reported_version: reported,
                reused: true,
            });
        }
        // Present but broken: fall through and reinstall over it.
        out.push("existing Node install did not answer — reinstalling".to_string());
    }

    let archive_name = platform.archive_name(NODE_VERSION);
    let archive_url = format!("{NODE_DIST_BASE}/v{NODE_VERSION}/{archive_name}");
    let shasums_url = format!("{NODE_DIST_BASE}/v{NODE_VERSION}/SHASUMS256.txt");

    // The published hash first: if we cannot verify, we do not download.
    let shasums = source.text(&shasums_url).await?;
    let expected = parse_shasums(&shasums, &archive_name)
        .ok_or_else(|| format!("SHASUMS256.txt does not list {archive_name}"))?;

    let downloads = root.join(".downloads");
    std::fs::create_dir_all(&downloads).map_err(|e| format!("cannot create downloads dir: {e}"))?;
    let archive_path = downloads.join(&archive_name);
    let mut progress = |_done: u64, _total: Option<u64>| {};
    source
        .download(&archive_url, &archive_path, &mut progress)
        .await?;

    let bytes =
        std::fs::read(&archive_path).map_err(|e| format!("cannot read downloaded archive: {e}"))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        // Poisoned or truncated — remove so a retry starts clean.
        let _ = std::fs::remove_file(&archive_path);
        return Err(format!(
            "sha256 mismatch for {archive_name}: expected {expected}, got {actual} — \
             the download was corrupted; re-run `smith setup`"
        ));
    }
    out.push(format!(
        "sha256 matched SHASUMS256.txt for {archive_name} (same origin as the bytes, so this \
         catches corruption, not tampering)"
    ));

    // Staged extraction with a per-call unique name: two concurrent setups
    // must not delete each other's half-written tree. (Learned the hard way
    // on the Chromium path.)
    let staging = root.join(format!(
        ".staging-node-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create staging dir: {e}"))?;

    let extract_result = if matches!(platform, NodePlatform::WinX64) {
        extract_zip(&bytes, &staging)
    } else {
        extract_tar_gz(&bytes, &staging)
    };
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Probe **before** the atomic rename: a tree that cannot run node must
    // never become the recorded install.
    let staged_binary = staging
        .join(platform.archive_root(NODE_VERSION))
        .join(platform.node_binary());
    let reported = match crate::runtime::probe_version(&staged_binary).await {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("the downloaded Node did not run: {e}"));
        }
    };
    if !reported.contains(NODE_VERSION) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "the downloaded Node reports {reported}, expected v{NODE_VERSION}"
        ));
    }

    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }
    if let Some(parent) = install_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create node dir: {e}"))?;
    }
    std::fs::rename(&staging, &install_dir)
        .map_err(|e| format!("cannot move Node into place: {e}"))?;
    let _ = std::fs::remove_file(&archive_path);

    Ok(ProvisionedNode {
        version: NODE_VERSION.to_string(),
        binary,
        reported_version: reported,
        reused: false,
    })
}

/// Unpacks a `.tar.gz`, refusing entries that escape `dest` — the same jail
/// property the Chromium zip path enforces, because a hostile archive is a
/// hostile archive whatever its framing.
fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| format!("unreadable tar archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("corrupt tar entry: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("tar entry with an unreadable path: {e}"))?
            .to_string_lossy()
            .into_owned();
        let Some(target) = safe_entry_path(dest, &name) else {
            return Err(format!("tar entry escapes the extraction dir: {name}"));
        };
        // `Entry::unpack` does not create parents (that is `unpack_in`'s
        // job, which we are not using because the jail decision is ours).
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot mkdir for {name}: {e}"))?;
        }
        entry
            .unpack(&target)
            .map_err(|e| format!("cannot unpack {name}: {e}"))?;
    }
    Ok(())
}

/// Unpacks a `.zip` through the same jail.
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("unreadable zip archive: {e}"))?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("corrupt zip entry: {e}"))?;
        let name = file.name().to_string();
        let Some(target) = safe_entry_path(dest, &name) else {
            return Err(format!("zip entry escapes the extraction dir: {name}"));
        };
        if name.ends_with('/') {
            std::fs::create_dir_all(&target).map_err(|e| format!("cannot mkdir {name}: {e}"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot mkdir for {name}: {e}"))?;
        }
        let mut out_file =
            std::fs::File::create(&target).map_err(|e| format!("cannot create {name}: {e}"))?;
        std::io::copy(&mut file, &mut out_file).map_err(|e| format!("cannot write {name}: {e}"))?;
    }
    Ok(())
}

/// The outcome of a successful `provision_ninerouter`.
#[derive(Debug, Clone)]
pub struct ProvisionedNineRouter {
    pub version: String,
    /// The gateway's CLI entry (`…/node_modules/9router/cli.js`), run as
    /// `node cli.js` — portable, no `.bin` symlinks or Windows `.cmd` shims.
    pub cli: PathBuf,
    pub reused: bool,
}

/// Where the gateway package lives under the runtime root.
fn ninerouter_dir(root: &Path) -> PathBuf {
    root.join("9router")
}

/// The gateway's CLI entry inside an install dir. From the published package:
/// `"bin": {"9router": "cli.js"}`.
pub fn ninerouter_cli(dir: &Path) -> PathBuf {
    dir.join("node_modules").join("9router").join("cli.js")
}

/// Installs the pinned gateway with the provisioned npm.
pub async fn provision_ninerouter(
    node: &Path,
    root: &Path,
    out: &mut Vec<String>,
) -> Result<ProvisionedNineRouter, String> {
    let platform = NodePlatform::detect()
        .ok_or_else(|| format!("unsupported platform: {}", std::env::consts::ARCH))?;
    let dir = ninerouter_dir(root);
    let cli = ninerouter_cli(&dir);

    if cli.is_file() {
        return Ok(ProvisionedNineRouter {
            version: NINEROUTER_VERSION.to_string(),
            cli,
            reused: true,
        });
    }

    // node sits at <install>/<archive_root>/bin/node; npm-cli.js is relative
    // to the same tree root.
    let tree_root = if matches!(platform, NodePlatform::WinX64) {
        node.parent()
    } else {
        node.parent().and_then(Path::parent)
    }
    .ok_or_else(|| "cannot locate the Node tree around the binary".to_string())?;
    let npm_cli = tree_root.join(platform.npm_cli());
    if !npm_cli.is_file() {
        return Err(format!(
            "npm not found beside the provisioned Node ({}) — re-run `smith setup`",
            npm_cli.display()
        ));
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create 9router dir: {e}"))?;
    out.push(format!("installing 9router@{NINEROUTER_VERSION}…"));

    let output = tokio::process::Command::new(node)
        .arg(&npm_cli)
        .arg("install")
        .arg("--prefix")
        .arg(&dir)
        .arg("--no-fund")
        .arg("--no-audit")
        .arg("--loglevel=error")
        .arg(format!("9router@{NINEROUTER_VERSION}"))
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not run npm: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "npm install 9router@{NINEROUTER_VERSION} failed ({}):\n{}",
            output.status,
            stderr.trim()
        ));
    }
    if !cli.is_file() {
        return Err(format!(
            "npm reported success but {} does not exist — the package layout may have changed; \
             please report this",
            cli.display()
        ));
    }

    Ok(ProvisionedNineRouter {
        version: NINEROUTER_VERSION.to_string(),
        cli,
        reused: false,
    })
}

/// The Node binary to use: env override → provisioned → PATH. Mirrors
/// `runtime::find_browser`'s precedence for the same reasons.
pub fn find_node(settings: &smith_config::RuntimeSettings) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SMITH_NODE_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(path) = settings.node_path.as_deref() {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    which_node()
}

fn which_node() -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether the gateway answers on its port. Half a second: this gates the
/// spawn decision at session start, and a hung probe would cost every
/// startup what it saves one.
pub async fn ninerouter_healthy(base_url: &str) -> bool {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().as_u16() < 500)
}

/// Starts the gateway if it is not already answering.
///
/// Spawned **detached**, like `ensure_ollama_running`: the gateway is a
/// machine-level daemon. Owning it as a child would make two concurrent smith
/// sessions fight over its lifetime, and killing it on exit would throw away
/// the next session's instant start.
///
/// Missing prerequisites are an error naming `smith setup` — never a silent
/// skip, because a fallback that quietly is not there is discovered at the
/// worst possible moment.
pub async fn ensure_ninerouter_running(config: &Config) -> Result<(), String> {
    let base_url = config
        .nine_router
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
    if ninerouter_healthy(&base_url).await {
        return Ok(());
    }

    let node = find_node(&config.runtime).ok_or_else(|| {
        "9router needs Node, and none is provisioned — run `smith setup`".to_string()
    })?;
    let cli = config
        .runtime
        .ninerouter_dir
        .as_deref()
        .map(|dir| ninerouter_cli(Path::new(dir)))
        .filter(|cli| cli.is_file())
        .ok_or_else(|| "the 9router gateway is not installed — run `smith setup`".to_string())?;

    let port = port_of(&base_url).unwrap_or(NINEROUTER_PORT);
    tokio::process::Command::new(&node)
        .arg(&cli)
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start the 9router gateway: {e}"))?;

    // Poll rather than sleep-once: a Next.js app takes a few seconds to bind,
    // and how many depends on the machine.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if ninerouter_healthy(&base_url).await {
            return Ok(());
        }
    }
    Err(format!(
        "the 9router gateway did not answer on {base_url} within 10s — check `smith doctor`"
    ))
}

/// Port of an `http://host:port/...` URL, without pulling in a URL crate.
fn port_of(base_url: &str) -> Option<u16> {
    let rest = base_url.split("://").nth(1)?;
    let host_port = rest.split('/').next()?;
    host_port.rsplit_once(':')?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_table_matches_nodejs_dist_naming() {
        let cases = [
            (("linux", "x86_64"), "node-v24.19.0-linux-x64.tar.gz"),
            (("linux", "aarch64"), "node-v24.19.0-linux-arm64.tar.gz"),
            (("macos", "aarch64"), "node-v24.19.0-darwin-arm64.tar.gz"),
            (("windows", "x86_64"), "node-v24.19.0-win-x64.zip"),
        ];
        for ((os, arch), expected) in cases {
            let platform = NodePlatform::for_target(os, arch).unwrap();
            assert_eq!(platform.archive_name("24.19.0"), expected);
        }
        assert!(NodePlatform::for_target("freebsd", "x86_64").is_none());
    }

    #[test]
    fn shasums_parsing_finds_the_right_line_and_rejects_junk() {
        let text = "\
abc123def456abc123def456abc123def456abc123def456abc123def456abcd  node-v24.19.0-linux-x64.tar.gz
0000000000000000000000000000000000000000000000000000000000000000  node-v24.19.0-win-x64.zip
";
        assert_eq!(
            parse_shasums(text, "node-v24.19.0-linux-x64.tar.gz").as_deref(),
            Some("abc123def456abc123def456abc123def456abc123def456abc123def456abcd")
        );
        assert!(parse_shasums(text, "node-v24.19.0-darwin-x64.tar.gz").is_none());
        // A malformed hash is not a hash.
        assert!(parse_shasums("nothex  file.tar.gz", "file.tar.gz").is_none());
        assert!(parse_shasums("", "anything").is_none());
    }

    /// The same property the Chromium zip test pins, for tar: a hostile
    /// archive must not write outside the extraction dir.
    #[test]
    fn tar_entries_escaping_the_destination_are_refused() {
        // `tar::Builder` itself refuses `..` in `append_data`, so the hostile
        // name is written straight into the header bytes — which is exactly
        // what an attacker's hand-rolled archive would contain.
        let payload = b"owned";
        let mut header = tar::Header::new_gnu();
        let name = b"../../escape.txt";
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_size(payload.len() as u64);
        header.set_cksum();

        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(payload);
        // Pad the data block and append the end-of-archive blocks.
        raw.resize(512 + 512, 0);
        raw.resize(raw.len() + 1024, 0);

        let bytes = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, &raw).unwrap();
            encoder.finish().unwrap()
        };

        let dir = tempfile::tempdir().unwrap();
        let err = extract_tar_gz(&bytes, dir.path()).expect_err("must refuse the escape");
        // Ours or the tar crate's own guard — either way the write must not
        // have happened. Both layers on purpose: defense in depth.
        assert!(err.contains("escapes") || err.contains(".."), "{err}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn a_wellformed_tar_extracts_where_it_says() {
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"#!/bin/sh\necho v24.19.0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                "node-v24.19.0-linux-x64/bin/node",
                payload.as_slice(),
            )
            .unwrap();
        let bytes = {
            let inner = builder.into_inner().unwrap();
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, &inner).unwrap();
            encoder.finish().unwrap()
        };

        let dir = tempfile::tempdir().unwrap();
        extract_tar_gz(&bytes, dir.path()).unwrap();
        assert!(dir
            .path()
            .join("node-v24.19.0-linux-x64/bin/node")
            .is_file());
    }

    #[test]
    fn the_port_is_read_out_of_a_base_url() {
        assert_eq!(port_of("http://localhost:20128/v1"), Some(20128));
        assert_eq!(port_of("http://127.0.0.1:9999"), Some(9999));
        assert_eq!(port_of("http://localhost/v1"), None);
    }

    #[tokio::test]
    async fn ensure_with_nothing_installed_errs_naming_setup() {
        // No gateway on this port and no recorded install — the error must
        // say what to run, not silently skip. Deliberately does NOT touch
        // PATH or any env var: env is process-global, tests run in parallel,
        // and blanking PATH here once broke an unrelated MCP test that was
        // spawning python3 at that exact moment.
        let mut config = Config::default();
        config.nine_router.base_url = Some("http://127.0.0.1:1".into());
        let err = ensure_ninerouter_running(&config)
            .await
            .map(|_| ())
            .expect_err("nothing is installed");
        assert!(err.contains("smith setup"), "{err}");
        assert!(err.contains("not installed"), "{err}");
    }

    /// Real download + sha256 + probe into a tempdir. ~50 MB from nodejs.org.
    ///
    /// `cargo test -p smith-cli live_node -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_node_provisioning_installs_and_probes() {
        let dir = tempfile::tempdir().unwrap();
        let source = crate::runtime::HttpAssetSource::new().unwrap();
        let mut out = Vec::new();
        let node = provision_node(&source, dir.path(), &mut out)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        println!(
            "node {} at {}",
            node.reported_version,
            node.binary.display()
        );
        assert!(node.reported_version.contains(NODE_VERSION));

        let gateway = provision_ninerouter(&node.binary, dir.path(), &mut out)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        println!("9router cli at {}", gateway.cli.display());
        assert!(gateway.cli.is_file());
    }
}
