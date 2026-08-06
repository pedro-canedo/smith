//! Provisioning the third-party binaries smith needs, so installing the CLI
//! is the only install anyone performs.
//!
//! Today that is one thing: a headless browser for `web_search`'s Chromium
//! tier (`smith-tools/src/chromium.rs`), which is otherwise dead on a machine
//! with no Chrome on `PATH`.
//!
//! # Why `smith setup` and not first use
//!
//! This runs as an explicit step in the setup wizard. Nothing is fetched
//! behind the user's back mid-turn: a 100 MB download that starts because a
//! model happened to call `web_search` is indistinguishable from a hang, and
//! it spends someone's bandwidth without asking. When the browser is missing
//! later, smith says "run `smith setup`" and falls through to its plain-HTTP
//! search tier instead.
//!
//! # Why not bundle it into the binary
//!
//! ~100 MB per platform inside the executable would break `cargo-dist`,
//! Homebrew and `cargo-binstall` alike. A cache under `~/.smith/runtime`
//! reaches the same user-visible place — install smith, install nothing else.
//!
//! # Why `chrome-headless-shell` rather than full Chrome
//!
//! Measured, not assumed. `chromium.rs` drives the browser with
//! `--headless … --dump-dom`. Against Chrome for Testing 151 the full `chrome`
//! asset never terminates under `--dump-dom` — not even on a `data:` URL —
//! while `chrome-headless-shell`, which is the same version of the *old*
//! headless implementation shipped as its own binary, answers in about a
//! second with the exact argument list `chromium.rs` builds. The
//! headless-shell asset is also ~40% smaller and unpacks to a single flat
//! directory on every platform, with no `.app` bundle and no symlinks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt as _;
use md5::{Digest as _, Md5};

/// Google's own index of Chrome for Testing builds. Verified live: a JSON
/// document of `{timestamp, channels: {Stable, Beta, Dev, Canary}}`, each
/// channel carrying `version`, `revision` and `downloads` keyed by asset
/// (`chrome`, `chromedriver`, `chrome-headless-shell`), each asset a list of
/// `{platform, url}`.
pub const CFT_MANIFEST_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

/// Stable, not Beta: a search backend is not where anyone wants to discover a
/// browser regression.
const CFT_CHANNEL: &str = "Stable";

/// The asset within a channel. See the module docs for why this one.
const CFT_ASSET: &str = "chrome-headless-shell";

/// Subdirectory of `~/.smith/runtime` holding every installed build. Named
/// after the asset so a future runtime (a language server, a formatter)
/// lands beside it rather than on top of it.
const INSTALL_SUBDIR: &str = "chrome-headless-shell";

/// Where a partially downloaded archive waits between attempts.
const DOWNLOADS_SUBDIR: &str = ".downloads";

/// Prefix for the directory an archive is unpacked into before it is renamed
/// into place. The leading dot keeps it out of the way, and the prefix is
/// what `sweep_stale_staging` recognises.
const STAGING_PREFIX: &str = ".staging-";

/// How long the freshly installed binary gets to answer `--version`. It is a
/// cold start of a large executable; generous, but bounded, because a browser
/// that never exits must fail the install rather than wedge `smith setup`.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum gap between progress repaints. Fast enough to look live, slow
/// enough that a redirected stdout doesn't collect thousands of lines.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// A download that made no progress at all in this long is treated as dead.
/// The whole-transfer alternative would have to be sized for the slowest
/// plausible link, which on a fast one means waiting minutes to notice a
/// stall.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The platforms Chrome for Testing publishes. Verified against the live
/// manifest: exactly these five, with no `linux-arm64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CftPlatform {
    Linux64,
    MacArm64,
    MacX64,
    Win32,
    Win64,
}

impl CftPlatform {
    /// The platform this build of smith is running on, or `None` where Chrome
    /// for Testing publishes nothing — notably 64-bit ARM Linux, which is a
    /// real machine people run smith on and which has no asset at all.
    pub fn detect() -> Option<Self> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Split from `detect` so the mapping is testable on one machine. The
    /// strings are `std::env::consts::{OS, ARCH}` values.
    pub fn for_target(os: &str, arch: &str) -> Option<Self> {
        Some(match (os, arch) {
            ("linux", "x86_64") => Self::Linux64,
            ("macos", "aarch64") => Self::MacArm64,
            ("macos", "x86_64") => Self::MacX64,
            ("windows", "x86_64") => Self::Win64,
            ("windows", "x86") => Self::Win32,
            _ => return None,
        })
    }

    /// The `platform` key used inside the manifest, and the suffix of every
    /// asset URL and archive root for that platform.
    pub fn key(self) -> &'static str {
        match self {
            Self::Linux64 => "linux64",
            Self::MacArm64 => "mac-arm64",
            Self::MacX64 => "mac-x64",
            Self::Win32 => "win32",
            Self::Win64 => "win64",
        }
    }

    /// The single top-level directory the archive unpacks into. Verified by
    /// reading the central directory of all five published archives: every
    /// one has exactly one top-level entry, named for the asset and platform.
    pub fn archive_root(self) -> String {
        format!("{CFT_ASSET}-{}", self.key())
    }

    /// The executable inside `archive_root`.
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Win32 | Self::Win64 => "chrome-headless-shell.exe",
            _ => "chrome-headless-shell",
        }
    }

    /// Whether a Unix executable bit has to be restored on extraction. Zip
    /// stores the mode, but only Unix acts on it.
    fn is_windows(self) -> bool {
        matches!(self, Self::Win32 | Self::Win64)
    }
}

/// One build of one asset for one platform, as read out of the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CftBuild {
    pub version: String,
    pub url: String,
}

/// What could be said about the bytes that arrived.
///
/// This distinction is the whole point of the type: `Md5Verified` is a real
/// check against a hash the storage layer computed over the stored object,
/// and it catches truncation, a resumed range stitched together wrongly, and
/// disk corruption. It is **not** a signature and not an independent
/// attestation — the hash and the bytes come from the same origin over the
/// same TLS connection, so it proves nothing against that origin itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// Google Cloud Storage served an `x-goog-hash: md5=…` for the object and
    /// the downloaded bytes hash to it.
    Md5Verified,
    /// No checksum was served. Only the byte count was checked.
    LengthOnly,
}

impl Integrity {
    /// Wording for the setup transcript. Deliberately says what was and was
    /// not established rather than the word "verified" on its own.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Md5Verified => {
                "MD5 matched the checksum Google Cloud Storage serves for the object \
                 (same origin as the bytes, so this catches corruption, not tampering)"
            }
            Self::LengthOnly => {
                "no checksum was published for this archive — only its byte count was checked"
            }
        }
    }
}

/// The outcome of a successful `provision_chromium`.
#[derive(Debug, Clone)]
pub struct Provisioned {
    pub version: String,
    pub binary: PathBuf,
    /// The version string the installed binary reported when asked, which is
    /// the only proof the archive contained a working browser.
    pub reported_version: String,
    /// `None` when the install was already present and nothing was fetched.
    pub integrity: Option<Integrity>,
    pub reused: bool,
}

/// Bytes that landed on disk plus whatever the transport said about them.
#[derive(Debug, Clone)]
pub struct Downloaded {
    pub bytes: u64,
    /// Base64 MD5 from `x-goog-hash`, when the server offered one.
    pub md5_base64: Option<String>,
}

/// Everything that touches the network, behind a trait so the tests can drive
/// the whole install — resume decision, extraction, layout check, atomic
/// rename, config value — against a fixture archive without a socket.
#[async_trait]
pub trait AssetSource: Send + Sync {
    async fn manifest(&self) -> Result<String, String>;

    /// Fetches `url` into `dest`, resuming if `dest` already holds a prefix.
    /// `progress` is called with (bytes on disk, total if known).
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<Downloaded, String>;
}

// ---------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------

/// Picks this platform's `chrome-headless-shell` build out of the manifest.
pub fn parse_manifest(json: &str, platform: CftPlatform) -> Result<CftBuild, String> {
    let doc: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("Chrome for Testing manifest is not JSON: {e}"))?;
    let channel = doc
        .get("channels")
        .and_then(|c| c.get(CFT_CHANNEL))
        .ok_or_else(|| format!("manifest has no `{CFT_CHANNEL}` channel"))?;
    let version = channel
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("manifest channel has no version")?
        .to_string();
    let downloads = channel
        .get("downloads")
        .and_then(|d| d.get(CFT_ASSET))
        .and_then(|a| a.as_array())
        .ok_or_else(|| format!("manifest has no `{CFT_ASSET}` downloads"))?;
    let url = downloads
        .iter()
        .find(|entry| entry.get("platform").and_then(|p| p.as_str()) == Some(platform.key()))
        .and_then(|entry| entry.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| {
            format!(
                "Chrome for Testing publishes no `{CFT_ASSET}` build for {}",
                platform.key()
            )
        })?
        .to_string();
    Ok(CftBuild { version, url })
}

/// Directory name for one installed build.
///
/// Versioned so an upgrade is a *new* directory rather than an overwrite: a
/// half-written update on top of a working install would leave neither
/// usable, and this is the cheapest way for that never to be possible.
pub fn version_dir_name(version: &str, platform: CftPlatform) -> String {
    format!("{}-{}", sanitize_component(version), platform.key())
}

/// Keeps a manifest value from escaping the install root or naming something
/// the filesystem refuses. Versions are `151.0.7922.76` in practice, but this
/// string comes off the network and is about to become a path.
fn sanitize_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `.` and `..` are the two names that would resolve somewhere else
    // entirely, and both survive the filter above.
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return "unknown".to_string();
    }
    cleaned
}

/// What to do with a partial archive left behind by an earlier attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// The file is already the whole archive.
    Complete,
    /// Ask for the remainder starting at this offset.
    From(u64),
    /// Throw it away and start over.
    Restart,
}

/// Decides how to continue a download, given what's on disk and what the
/// server says the whole thing weighs.
///
/// Longer-than-expected means the file is not a prefix of this archive at all
/// — a different build under a reused name, or a corrupted write — and
/// resuming from its end would silently produce a corrupt zip. Restarting is
/// the only safe reading. An unknown total is the same situation with less
/// information, so it gets the same answer.
pub fn resume_plan(on_disk: u64, total: Option<u64>) -> Resume {
    match total {
        _ if on_disk == 0 => Resume::Restart,
        Some(total) if on_disk == total => Resume::Complete,
        Some(total) if on_disk < total => Resume::From(on_disk),
        Some(_) => Resume::Restart,
        None => Resume::Restart,
    }
}

/// Pulls the base64 MD5 out of the `x-goog-hash` header(s).
///
/// GCS sends this either as repeated headers or as one comma-joined value,
/// and always alongside a `crc32c=` entry, so both shapes have to be scanned
/// rather than parsed positionally.
pub fn parse_goog_hash_md5<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<String> {
    values
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter_map(|part| part.trim().strip_prefix("md5="))
        .map(|s| s.to_string())
        .next()
}

/// A human-readable byte count. Whole MB once past a megabyte, because a
/// tenth of a byte in a progress line is noise.
fn human_bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

/// One progress line. Pure so its shape is testable — a silent 100 MB
/// download is indistinguishable from a hang, and this is the thing that
/// makes the difference.
pub fn progress_line(done: u64, total: Option<u64>, elapsed: Duration) -> String {
    let rate = if elapsed.as_secs_f64() > 0.0 {
        format!(
            "{}/s",
            human_bytes((done as f64 / elapsed.as_secs_f64()) as u64)
        )
    } else {
        "—".to_string()
    };
    match total {
        // Clamped: a server that under-reports its own length must not make
        // this print 103%.
        Some(total) if total > 0 => {
            let pct = ((done as f64 / total as f64) * 100.0).min(100.0);
            format!(
                "  {:>9} / {:>9}  {pct:>5.1}%  {rate:>10}",
                human_bytes(done),
                human_bytes(total)
            )
        }
        // Length unknown: show what has arrived rather than a fake percentage.
        _ => format!("  {:>9}  {rate:>10}", human_bytes(done)),
    }
}

/// Rejects a zip entry whose name would write outside the extraction root.
///
/// A `../` in a member name is the classic zip-slip, and an absolute name or
/// a Windows drive letter is the same bug wearing a different hat. Chrome's
/// own archives contain none of these — which is exactly why a check here
/// costs nothing and is worth having anyway.
pub fn safe_entry_path(root: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let mut out = root.to_path_buf();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            // A bare drive letter, or anything the OS would treat as a root.
            p if p.ends_with(':') => return None,
            p => out.push(p),
        }
    }
    (out != root).then_some(out)
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Downloads, verifies and installs a headless browser under `root`, printing
/// progress to `out`.
///
/// Nothing is visible at the final path until the archive has been unpacked
/// *and* the binary inside it has answered `--version`, so an interrupted run
/// can never leave behind a directory that later looks installed.
pub async fn provision_chromium(
    source: &dyn AssetSource,
    root: &Path,
    out: &mut (dyn Write + Send),
) -> Result<Provisioned, String> {
    let platform = CftPlatform::detect().ok_or_else(|| {
        format!(
            "Chrome for Testing publishes no build for {}/{} — install a Chromium-family browser \
             with your package manager and smith will find it on PATH, or point \
             SMITH_CHROMIUM_PATH at one",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let install_root = root.join(INSTALL_SUBDIR);
    std::fs::create_dir_all(&install_root)
        .map_err(|e| format!("cannot create {}: {e}", install_root.display()))?;
    // Anything left over from a run that died mid-extract. Swept before the
    // new attempt so a crashed install doesn't accumulate copies of a browser.
    sweep_stale_staging(&install_root);

    let _ = writeln!(out, "Looking up the latest Chrome for Testing build...");
    let manifest = source.manifest().await?;
    let build = parse_manifest(&manifest, platform)?;
    let dest = install_root.join(version_dir_name(&build.version, platform));
    let binary = dest.join(platform.binary_name());

    if let Ok(reported) = probe_version(&binary).await {
        let _ = writeln!(
            out,
            "Already installed: {} ({})",
            build.version,
            binary.display()
        );
        return Ok(Provisioned {
            version: build.version,
            binary,
            reported_version: reported,
            integrity: None,
            reused: true,
        });
    }
    // Present but not runnable: a truncated or wrong-architecture install from
    // a previous attempt. Reinstalling over it is the fix, and it has to go
    // before the rename can land.
    if dest.exists() {
        let _ = writeln!(
            out,
            "An install at {} does not run — replacing it.",
            dest.display()
        );
        let _ = std::fs::remove_dir_all(&dest);
    }

    let downloads = install_root.join(DOWNLOADS_SUBDIR);
    std::fs::create_dir_all(&downloads)
        .map_err(|e| format!("cannot create {}: {e}", downloads.display()))?;
    let archive = downloads.join(format!(
        "{}.zip",
        version_dir_name(&build.version, platform)
    ));

    let _ = writeln!(out, "Downloading {} {}", CFT_ASSET, build.version);
    let started = Instant::now();
    let mut last_paint = Instant::now() - PROGRESS_INTERVAL;
    let downloaded = source
        .download(&build.url, &archive, &mut |done, total| {
            if last_paint.elapsed() < PROGRESS_INTERVAL {
                return;
            }
            last_paint = Instant::now();
            let _ = write!(out, "\r{}", progress_line(done, total, started.elapsed()));
            let _ = out.flush();
        })
        .await?;
    let _ = writeln!(
        out,
        "\r{}",
        progress_line(downloaded.bytes, Some(downloaded.bytes), started.elapsed())
    );

    let integrity = verify_archive(&archive, downloaded.bytes, downloaded.md5_base64.as_deref())?;
    let _ = writeln!(out, "Checked: {}", integrity.describe());

    let staging = install_root.join(staging_dir_name());
    let _ = std::fs::remove_dir_all(&staging);
    let unpacked = (|| {
        let _ = writeln!(out, "Extracting...");
        extract_zip(&archive, &staging, platform)?;
        let unpacked = staging.join(platform.archive_root());
        let staged_binary = unpacked.join(platform.binary_name());
        if !staged_binary.is_file() {
            return Err(format!(
                "the archive did not contain {}/{} — Chrome for Testing may have changed its \
                 layout; please report this",
                platform.archive_root(),
                platform.binary_name()
            ));
        }
        Ok(unpacked)
    })();
    let unpacked = match unpacked {
        Ok(u) => u,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // The real acceptance test: an executable that starts and identifies
    // itself. Run before the rename, so a browser that cannot run never
    // occupies the path that means "installed".
    let staged_binary = unpacked.join(platform.binary_name());
    let reported = match probe_version(&staged_binary).await {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!(
                "the downloaded browser did not run: {e}\n  On Linux a headless Chrome still \
                 needs a few shared libraries; on Debian/Ubuntu \
                 `sudo apt-get install -y libnss3 libatk-bridge2.0-0 libgbm1 libasound2` \
                 covers the usual gaps."
            ));
        }
    };

    // Rename, not copy: on one filesystem it is atomic, so `dest` either does
    // not exist or is a complete, already-verified install.
    std::fs::rename(&unpacked, &dest).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        format!("could not install into {}: {e}", dest.display())
    })?;
    let _ = std::fs::remove_dir_all(&staging);
    // Only now is the archive worth discarding: until the install landed it
    // was the thing a retry could resume from.
    let _ = std::fs::remove_file(&archive);

    Ok(Provisioned {
        version: build.version,
        binary: dest.join(platform.binary_name()),
        reported_version: reported,
        integrity: Some(integrity),
        reused: false,
    })
}

/// A staging directory name unique to *this call*, not just this process.
///
/// The pid alone is not enough: two `provision_chromium` calls in one process
/// would share a name, and the callee wipes the directory before extracting —
/// so one call would delete the other's half-unpacked archive. That is not
/// hypothetical, it is a test suite running its provisioning cases in
/// parallel threads.
fn staging_dir_name() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}{}-{n}", STAGING_PREFIX, std::process::id())
}

/// The prefix every staging directory of *this* process shares.
fn staging_prefix_for_this_process() -> String {
    format!("{}{}-", STAGING_PREFIX, std::process::id())
}

/// Removes `.staging-*` directories from earlier runs. Best effort: one still
/// in use by a concurrent `smith setup` will refuse to go, and that is fine.
///
/// Everything belonging to this process is skipped as a group — a sibling call
/// may be extracting into one of them right now.
fn sweep_stale_staging(install_root: &Path) {
    let Ok(entries) = std::fs::read_dir(install_root) else {
        return;
    };
    let mine = staging_prefix_for_this_process();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(STAGING_PREFIX) && !name.starts_with(&mine) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Checks the bytes on disk against what the server said about them.
fn verify_archive(
    archive: &Path,
    expected_len: u64,
    expected_md5_base64: Option<&str>,
) -> Result<Integrity, String> {
    let actual_len = std::fs::metadata(archive)
        .map_err(|e| format!("cannot stat the downloaded archive: {e}"))?
        .len();
    if actual_len != expected_len {
        return Err(format!(
            "the download is {actual_len} bytes but {expected_len} were expected — \
             re-run `smith setup`, which resumes where this left off"
        ));
    }
    let Some(expected) = expected_md5_base64 else {
        return Ok(Integrity::LengthOnly);
    };
    let actual = md5_base64_of(archive)?;
    if actual != expected {
        // The partial file is the corrupt thing; leaving it would make every
        // retry resume into the same failure.
        let _ = std::fs::remove_file(archive);
        return Err(
            "the downloaded archive does not match the checksum its server published — \
             the partial download has been discarded; re-run `smith setup` to fetch it again"
                .to_string(),
        );
    }
    Ok(Integrity::Md5Verified)
}

fn md5_base64_of(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Md5::new();
    // Streamed rather than read to a Vec: the archive is ~100 MB and there is
    // no reason for all of it to be resident at once.
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(hasher.finalize()))
}

/// Unpacks `archive` into `into`, refusing any member that would write
/// outside it.
fn extract_zip(archive: &Path, into: &Path, platform: CftPlatform) -> Result<(), String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("cannot open {}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("the download is not a readable zip archive: {e}"))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("cannot read archive entry {i}: {e}"))?;
        let name = entry.name().to_string();
        let Some(path) = safe_entry_path(into, &name) else {
            return Err(format!(
                "the archive contains an entry that would write outside the install directory \
                 ({name}) — refusing to extract it"
            ));
        };
        if name.ends_with('/') {
            std::fs::create_dir_all(&path)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mode = entry.unix_mode();
        let mut sink = std::fs::File::create(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        std::io::copy(&mut entry, &mut sink)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        drop(sink);
        restore_mode(&path, mode, platform);
    }
    Ok(())
}

/// Puts back the executable bit the zip recorded.
///
/// Without this the extracted browser is a 200 MB file nobody can run. The
/// mode is taken from the archive rather than applied blanket-wise, with a
/// fallback for the one file that must be executable no matter what the
/// archive claims.
#[cfg(unix)]
fn restore_mode(path: &Path, mode: Option<u32>, platform: CftPlatform) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = mode.unwrap_or_else(|| {
        let is_binary = path.file_name().and_then(|n| n.to_str()) == Some(platform.binary_name());
        if is_binary || platform.is_windows() {
            0o755
        } else {
            0o644
        }
    });
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restore_mode(_path: &Path, _mode: Option<u32>, _platform: CftPlatform) {}

/// Runs the binary's `--version` and returns what it printed.
///
/// This is what turns "some bytes landed on disk" into "a browser is
/// installed". A wrong-architecture binary, a truncated one, or one missing a
/// shared library all fail here rather than at the user's next `web_search`.
pub async fn probe_version(binary: &Path) -> Result<String, String> {
    if !binary.is_file() {
        return Err(format!("{} does not exist", binary.display()));
    }
    let run = tokio::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_PROBE_TIMEOUT, run)
        .await
        .map_err(|_| {
            format!(
                "--version did not answer within {}s",
                VERSION_PROBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("could not run {}: {e}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err(format!("{} --version printed nothing", binary.display()));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Finding whatever browser this machine already has
// ---------------------------------------------------------------------------

/// The environment variable `smith-tools` honours above everything else.
pub const BROWSER_PATH_ENV: &str = "SMITH_CHROMIUM_PATH";
/// The cross-tool convention it checks second.
pub const BROWSER_PATH_ENV_FALLBACK: &str = "CHROME_PATH";

/// Mirrors `smith_tools::chromium::BROWSER_CANDIDATES`, which is private to
/// that crate.
///
/// Duplicated rather than shared because `smith-tools` is owned elsewhere and
/// exports nothing for this. The cost of drift is bounded and one-directional:
/// `smith doctor` would report "no browser" for one that `web_search` can in
/// fact find, which is a wrong diagnosis but never a wrong *behaviour*. Making
/// `chromium.rs` expose its resolution is the right fix — see the note in the
/// report accompanying this change.
const SYSTEM_BROWSER_CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
    "brave-browser",
    "microsoft-edge",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
];

/// Where a browser came from, so both `setup` and `doctor` can say which one
/// `web_search` will actually use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSource {
    /// `SMITH_CHROMIUM_PATH` or `CHROME_PATH`; the name is the variable.
    Env(&'static str),
    /// `[runtime] chromium_path` — what `smith setup` provisioned.
    Provisioned,
    /// Found on `PATH`, or at a known macOS bundle path.
    System,
}

#[derive(Debug, Clone)]
pub struct FoundBrowser {
    pub path: PathBuf,
    pub source: BrowserSource,
}

/// Resolves the browser in the same precedence `smith-tools` uses, with the
/// provisioned one slotted in where `main.rs` injects it.
///
/// An explicit environment override wins outright — honoured verbatim even if
/// it points at nothing, because silently substituting a different browser
/// than the one someone named is worse than a clear failure.
pub fn find_browser(runtime: &smith_config::RuntimeSettings) -> Option<FoundBrowser> {
    find_browser_with(runtime, |name| std::env::var(name).ok(), resolve_in_path)
}

/// Split out so the precedence is testable without a browser on the machine
/// running the tests, and without mutating the process environment.
fn find_browser_with(
    runtime: &smith_config::RuntimeSettings,
    env: impl Fn(&str) -> Option<String>,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Option<FoundBrowser> {
    for var in [BROWSER_PATH_ENV, BROWSER_PATH_ENV_FALLBACK] {
        if let Some(value) = env(var)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            return Some(FoundBrowser {
                path: PathBuf::from(value),
                source: BrowserSource::Env(var),
            });
        }
    }
    if let Some(path) = runtime
        .chromium_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        return Some(FoundBrowser {
            path: PathBuf::from(path),
            source: BrowserSource::Provisioned,
        });
    }
    SYSTEM_BROWSER_CANDIDATES
        .iter()
        .find_map(|c| resolve(c))
        .map(|path| FoundBrowser {
            path,
            source: BrowserSource::System,
        })
}

/// Resolves a bare command name against `PATH`; a name that is already a path
/// is only checked for existence. Same rule as `smith-tools`.
fn resolve_in_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// The real, networked source
// ---------------------------------------------------------------------------

/// `Content-Length` straight off the wire.
fn content_length_header(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub struct HttpAssetSource {
    client: reqwest::Client,
}

impl HttpAssetSource {
    pub fn new() -> Result<Self, String> {
        reqwest::Client::builder()
            .user_agent(concat!("smith/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(|client| Self { client })
            .map_err(|e| format!("could not build an HTTP client: {e}"))
    }
}

#[async_trait]
impl AssetSource for HttpAssetSource {
    async fn manifest(&self) -> Result<String, String> {
        let response = self
            .client
            .get(CFT_MANIFEST_URL)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                format!(
                    "could not reach the Chrome for Testing index ({CFT_MANIFEST_URL}): {e}\n  \
                     Check your network or proxy settings and re-run `smith setup`."
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "the Chrome for Testing index answered {status} — try again shortly"
            ));
        }
        response
            .text()
            .await
            .map_err(|e| format!("could not read the Chrome for Testing index: {e}"))
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<Downloaded, String> {
        let on_disk = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        // A HEAD first, so the resume decision is made against the real
        // length rather than against whatever a ranged GET happens to admit.
        let head = self
            .client
            .head(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("could not reach {url}: {e}"))?;
        // Read the header rather than `Response::content_length()`: on a HEAD
        // there is no body, so reqwest reports `None` there and the resume
        // path would silently never engage.
        let total = content_length_header(head.headers());
        let published_md5 = parse_goog_hash_md5(
            head.headers()
                .get_all("x-goog-hash")
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );

        let mut written = match resume_plan(on_disk, total) {
            Resume::Complete => {
                progress(on_disk, total);
                return Ok(Downloaded {
                    bytes: on_disk,
                    md5_base64: published_md5,
                });
            }
            Resume::From(offset) => offset,
            Resume::Restart => {
                let _ = std::fs::remove_file(dest);
                0
            }
        };

        let mut request = self.client.get(url).timeout(Duration::from_secs(60 * 60));
        if written > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={written}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("could not download {url}: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("downloading {url} failed with {status}"));
        }
        // A server that ignored the range answers 200 with the whole file. The
        // only correct response is to stop appending and start over, because
        // otherwise the prefix already on disk gets a second copy stapled to
        // it.
        let appending = written > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if written > 0 && !appending {
            let _ = std::fs::remove_file(dest);
            written = 0;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(appending)
            .write(true)
            .truncate(!appending)
            .open(dest)
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;

        progress(written, total);
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::time::timeout(STALL_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    format!(
                        "the download stalled for {}s — re-run `smith setup`, which resumes \
                         from where it stopped",
                        STALL_TIMEOUT.as_secs()
                    )
                })?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| {
                format!(
                    "the download failed after {written} bytes: {e}\n  Re-run `smith setup` — \
                     it resumes from where it stopped."
                )
            })?;
            file.write_all(&chunk)
                .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
            written += chunk.len() as u64;
            progress(written, total);
        }
        file.flush()
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;

        Ok(Downloaded {
            bytes: written,
            md5_base64: published_md5,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the real endpoint, trimmed. Kept faithful to what was
    /// observed live on 2026-08-05 so a change in the real one shows up as a
    /// parse failure rather than as a silent wrong URL.
    const MANIFEST: &str = r#"{
      "timestamp": "2026-08-04T00:00:00.000Z",
      "channels": {
        "Stable": {
          "channel": "Stable",
          "version": "151.0.7922.76",
          "revision": "1654411",
          "downloads": {
            "chrome": [
              {"platform": "linux64", "url": "https://example.test/chrome-linux64.zip"}
            ],
            "chrome-headless-shell": [
              {"platform": "linux64", "url": "https://example.test/151/linux64/chrome-headless-shell-linux64.zip"},
              {"platform": "mac-arm64", "url": "https://example.test/151/mac-arm64/chrome-headless-shell-mac-arm64.zip"},
              {"platform": "mac-x64", "url": "https://example.test/151/mac-x64/chrome-headless-shell-mac-x64.zip"},
              {"platform": "win32", "url": "https://example.test/151/win32/chrome-headless-shell-win32.zip"},
              {"platform": "win64", "url": "https://example.test/151/win64/chrome-headless-shell-win64.zip"}
            ]
          }
        },
        "Beta": {"version": "152.0.0.1", "downloads": {"chrome-headless-shell": []}}
      }
    }"#;

    // -- platform mapping ---------------------------------------------------

    #[test]
    fn maps_every_platform_chrome_for_testing_publishes() {
        for (os, arch, expected, key) in [
            ("linux", "x86_64", CftPlatform::Linux64, "linux64"),
            ("macos", "aarch64", CftPlatform::MacArm64, "mac-arm64"),
            ("macos", "x86_64", CftPlatform::MacX64, "mac-x64"),
            ("windows", "x86_64", CftPlatform::Win64, "win64"),
            ("windows", "x86", CftPlatform::Win32, "win32"),
        ] {
            let platform = CftPlatform::for_target(os, arch);
            assert_eq!(platform, Some(expected), "{os}/{arch}");
            assert_eq!(platform.unwrap().key(), key);
        }
    }

    /// 64-bit ARM Linux is a machine people really run smith on and Chrome for
    /// Testing publishes nothing for it. Guessing an asset would 404; saying
    /// so lets the caller print advice instead.
    #[test]
    fn unpublished_targets_are_none_rather_than_a_guess() {
        assert_eq!(CftPlatform::for_target("linux", "aarch64"), None);
        assert_eq!(CftPlatform::for_target("linux", "arm"), None);
        assert_eq!(CftPlatform::for_target("freebsd", "x86_64"), None);
        assert_eq!(CftPlatform::for_target("windows", "aarch64"), None);
    }

    /// Verified by reading the central directory of all five published
    /// archives: one top-level directory named for the asset and platform,
    /// with the executable directly inside it.
    #[test]
    fn archive_layout_matches_what_the_published_archives_contain() {
        assert_eq!(
            CftPlatform::Linux64.archive_root(),
            "chrome-headless-shell-linux64"
        );
        assert_eq!(
            CftPlatform::MacArm64.archive_root(),
            "chrome-headless-shell-mac-arm64"
        );
        assert_eq!(
            CftPlatform::Win64.archive_root(),
            "chrome-headless-shell-win64"
        );
        assert_eq!(CftPlatform::Linux64.binary_name(), "chrome-headless-shell");
        assert_eq!(CftPlatform::MacX64.binary_name(), "chrome-headless-shell");
        assert_eq!(
            CftPlatform::Win32.binary_name(),
            "chrome-headless-shell.exe"
        );
    }

    // -- manifest -----------------------------------------------------------

    #[test]
    fn reads_the_stable_build_for_each_platform() {
        for (platform, tail) in [
            (
                CftPlatform::Linux64,
                "linux64/chrome-headless-shell-linux64.zip",
            ),
            (
                CftPlatform::MacArm64,
                "mac-arm64/chrome-headless-shell-mac-arm64.zip",
            ),
            (CftPlatform::Win64, "win64/chrome-headless-shell-win64.zip"),
        ] {
            let build = parse_manifest(MANIFEST, platform).unwrap();
            assert_eq!(build.version, "151.0.7922.76");
            assert!(build.url.ends_with(tail), "got {}", build.url);
        }
    }

    /// Beta ships ahead of Stable; picking it up by accident would put an
    /// unreleased browser behind every `web_search`.
    #[test]
    fn takes_the_stable_channel_not_whichever_comes_first() {
        assert_eq!(
            parse_manifest(MANIFEST, CftPlatform::Linux64)
                .unwrap()
                .version,
            "151.0.7922.76"
        );
    }

    #[test]
    fn a_manifest_missing_this_platform_says_so_instead_of_guessing_a_url() {
        let trimmed = MANIFEST.replace(
            r#"{"platform": "win64", "url": "https://example.test/151/win64/chrome-headless-shell-win64.zip"}"#,
            r#"{"platform": "other", "url": "https://example.test/x.zip"}"#,
        );
        let err = parse_manifest(&trimmed, CftPlatform::Win64).unwrap_err();
        assert!(err.contains("win64"), "{err}");
    }

    #[test]
    fn a_broken_manifest_is_an_error_not_a_panic() {
        assert!(parse_manifest("not json", CftPlatform::Linux64).is_err());
        assert!(parse_manifest("{}", CftPlatform::Linux64).is_err());
        assert!(parse_manifest(r#"{"channels":{}}"#, CftPlatform::Linux64).is_err());
        assert!(parse_manifest(
            r#"{"channels":{"Stable":{"version":"1"}}}"#,
            CftPlatform::Linux64
        )
        .is_err());
    }

    // -- version directory naming ------------------------------------------

    /// The point of versioning the directory: two builds coexist, so an
    /// upgrade never writes into the install that currently works.
    #[test]
    fn each_build_gets_its_own_directory() {
        let a = version_dir_name("151.0.7922.76", CftPlatform::Linux64);
        let b = version_dir_name("152.0.8000.1", CftPlatform::Linux64);
        assert_eq!(a, "151.0.7922.76-linux64");
        assert_ne!(a, b);
    }

    /// Same version, different platform, is still a different install — a
    /// shared cache directory (an NFS home, a synced profile) must not hand
    /// one machine another's binary.
    #[test]
    fn the_platform_is_part_of_the_directory_name() {
        assert_ne!(
            version_dir_name("151.0.7922.76", CftPlatform::MacArm64),
            version_dir_name("151.0.7922.76", CftPlatform::MacX64)
        );
    }

    /// The version string arrives over the network and immediately becomes a
    /// path component.
    #[test]
    fn a_hostile_version_cannot_escape_the_install_root() {
        for evil in ["../../etc", "..", ".", "/absolute", "a/b", "", "..\\..\\x"] {
            let name = version_dir_name(evil, CftPlatform::Linux64);
            // The real invariant: whatever it becomes is exactly one ordinary
            // component directly under the install root — never a separator to
            // descend through, never a `..` to climb out of.
            let joined = Path::new("/root").join(&name);
            assert_eq!(
                joined.parent(),
                Some(Path::new("/root")),
                "{evil} -> {name}"
            );
            assert_eq!(
                joined.components().count(),
                Path::new("/root").components().count() + 1,
                "{evil} -> {name} is not a single component"
            );
        }
    }

    // -- resume -------------------------------------------------------------

    #[test]
    fn nothing_on_disk_starts_from_the_beginning() {
        assert_eq!(resume_plan(0, Some(100)), Resume::Restart);
        assert_eq!(resume_plan(0, None), Resume::Restart);
    }

    #[test]
    fn a_partial_file_is_resumed_from_its_end() {
        assert_eq!(resume_plan(40, Some(100)), Resume::From(40));
    }

    #[test]
    fn a_complete_file_is_not_downloaded_again() {
        assert_eq!(resume_plan(100, Some(100)), Resume::Complete);
    }

    /// Longer than the archive means it is not a prefix of it. Appending to
    /// that would build a corrupt zip out of two different builds, so the
    /// only safe reading is "throw it away".
    #[test]
    fn an_over_long_partial_file_is_restarted_not_resumed() {
        assert_eq!(resume_plan(200, Some(100)), Resume::Restart);
    }

    /// Without a length there is no way to tell a prefix from a whole file,
    /// and resuming on a guess corrupts the archive.
    #[test]
    fn an_unknown_total_restarts_rather_than_guessing() {
        assert_eq!(resume_plan(40, None), Resume::Restart);
    }

    // -- checksum header ----------------------------------------------------

    /// Both shapes GCS actually emits: repeated headers, and one comma-joined
    /// value. Observed live as the repeated form.
    #[test]
    fn finds_the_md5_among_the_other_hashes() {
        assert_eq!(
            parse_goog_hash_md5(["crc32c=SOPVlA==", "md5=hC/AkWNMyAanurxaV9FTfA=="]),
            Some("hC/AkWNMyAanurxaV9FTfA==".to_string())
        );
        assert_eq!(
            parse_goog_hash_md5(["crc32c=SOPVlA==,md5=hC/AkWNMyAanurxaV9FTfA=="]),
            Some("hC/AkWNMyAanurxaV9FTfA==".to_string())
        );
    }

    #[test]
    fn no_md5_is_none_rather_than_a_wrong_value() {
        assert_eq!(parse_goog_hash_md5(["crc32c=SOPVlA=="]), None);
        assert_eq!(parse_goog_hash_md5(std::iter::empty()), None);
    }

    /// `LengthOnly` must never read as though a checksum was checked — the
    /// whole reason the two variants exist.
    #[test]
    fn the_integrity_wording_does_not_overclaim() {
        assert!(Integrity::LengthOnly.describe().contains("no checksum"));
        assert!(Integrity::Md5Verified.describe().contains("MD5"));
        // Says plainly what it cannot establish.
        assert!(Integrity::Md5Verified.describe().contains("not tampering"));
    }

    /// Regression: `Response::content_length()` is `None` on a HEAD (no
    /// body), which made `resume_plan` see an unknown total and restart every
    /// download from zero. The header is the only thing that answers.
    #[test]
    fn the_content_length_comes_off_the_header_not_the_body() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            "120172907".parse().unwrap(),
        );
        assert_eq!(content_length_header(&headers), Some(120_172_907));

        assert_eq!(
            content_length_header(&reqwest::header::HeaderMap::new()),
            None
        );
    }

    // -- progress -----------------------------------------------------------

    #[test]
    fn progress_shows_how_far_along_a_known_length_download_is() {
        let line = progress_line(
            50 * 1024 * 1024,
            Some(100 * 1024 * 1024),
            Duration::from_secs(5),
        );
        assert!(line.contains("50.0 MB"), "{line}");
        assert!(line.contains("100.0 MB"), "{line}");
        assert!(line.contains("50.0%"), "{line}");
        assert!(line.contains("/s"), "{line}");
    }

    #[test]
    fn progress_without_a_known_length_still_shows_movement() {
        let line = progress_line(1024 * 1024, None, Duration::from_secs(1));
        assert!(line.contains("1.0 MB"), "{line}");
        assert!(!line.contains('%'), "no fake percentage: {line}");
    }

    /// A server that under-reports its length must not produce "137.2%".
    #[test]
    fn progress_never_exceeds_one_hundred_percent() {
        let line = progress_line(200, Some(100), Duration::from_secs(1));
        assert!(line.contains("100.0%"), "{line}");
    }

    #[test]
    fn progress_at_zero_elapsed_does_not_divide_by_zero() {
        let line = progress_line(0, Some(100), Duration::ZERO);
        assert!(line.contains("0.0%"), "{line}");
    }

    // -- zip entry safety ---------------------------------------------------

    #[test]
    fn a_normal_entry_lands_under_the_root() {
        let root = Path::new("/install");
        assert_eq!(
            safe_entry_path(root, "chrome-headless-shell-linux64/chrome-headless-shell"),
            Some(PathBuf::from(
                "/install/chrome-headless-shell-linux64/chrome-headless-shell"
            ))
        );
    }

    #[test]
    fn an_entry_that_would_escape_the_root_is_refused() {
        let root = Path::new("/install");
        for evil in [
            "../outside",
            "a/../../outside",
            "..\\..\\outside",
            "C:/Windows/system32/x",
            "",
            ".",
        ] {
            assert_eq!(safe_entry_path(root, evil), None, "accepted {evil:?}");
        }
    }

    /// A leading `/` is a path component to skip, not a new root — joining it
    /// naively is how an absolute member name escapes.
    #[test]
    fn an_absolute_entry_name_is_reanchored_under_the_root() {
        let root = Path::new("/install");
        let path = safe_entry_path(root, "/etc/passwd").unwrap();
        assert!(path.starts_with("/install"), "{}", path.display());
    }

    // -- browser resolution -------------------------------------------------

    fn runtime_with(path: Option<&str>) -> smith_config::RuntimeSettings {
        smith_config::RuntimeSettings {
            chromium_path: path.map(str::to_string),
            chromium_version: None,
        }
    }

    #[test]
    fn an_env_override_beats_the_provisioned_browser() {
        let found = find_browser_with(
            &runtime_with(Some("/home/u/.smith/runtime/chrome")),
            |v| (v == BROWSER_PATH_ENV).then(|| "/opt/mine".to_string()),
            |_| Some(PathBuf::from("/usr/bin/chromium")),
        )
        .unwrap();
        assert_eq!(found.path, PathBuf::from("/opt/mine"));
        assert_eq!(found.source, BrowserSource::Env(BROWSER_PATH_ENV));
    }

    /// smith's own variable wins over the shared one, matching `chromium.rs`.
    #[test]
    fn smiths_own_variable_wins_over_the_shared_one() {
        let found = find_browser_with(
            &runtime_with(None),
            |v| Some(format!("/from/{v}")),
            |_| None,
        )
        .unwrap();
        assert_eq!(
            found.path,
            PathBuf::from(format!("/from/{BROWSER_PATH_ENV}"))
        );
    }

    /// The provisioned browser is chosen over one on `PATH`: it is the build
    /// `smith setup` verified against the exact arguments `web_search` uses.
    #[test]
    fn a_provisioned_browser_beats_one_on_the_path() {
        let found = find_browser_with(
            &runtime_with(Some("/home/u/.smith/runtime/chrome")),
            |_| None,
            |_| Some(PathBuf::from("/usr/bin/chromium")),
        )
        .unwrap();
        assert_eq!(found.source, BrowserSource::Provisioned);
    }

    #[test]
    fn a_system_browser_is_used_when_nothing_else_is_configured() {
        let found = find_browser_with(
            &runtime_with(None),
            |_| None,
            |name| (name == "google-chrome").then(|| PathBuf::from("/usr/bin/google-chrome")),
        )
        .unwrap();
        assert_eq!(found.path, PathBuf::from("/usr/bin/google-chrome"));
        assert_eq!(found.source, BrowserSource::System);
    }

    #[test]
    fn no_browser_anywhere_is_none() {
        assert!(find_browser_with(&runtime_with(None), |_| None, |_| None).is_none());
    }

    /// An empty value is "unset", not "use the empty path" — the same
    /// tolerance `chromium.rs` applies.
    #[test]
    fn blank_settings_fall_through_rather_than_resolving_to_nothing() {
        let found = find_browser_with(
            &runtime_with(Some("  ")),
            |_| Some("   ".to_string()),
            |_| Some(PathBuf::from("/usr/bin/chromium")),
        )
        .unwrap();
        assert_eq!(found.source, BrowserSource::System);
    }

    // -- the whole install, against a fixture archive -----------------------

    /// Builds a zip in the shape the real archives have, with `binary` as the
    /// contents of the one executable.
    fn fixture_archive(platform: CftPlatform, binary: &[u8]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let root = platform.archive_root();
            zip.add_directory(format!("{root}/"), SimpleFileOptions::default())
                .unwrap();
            zip.start_file(
                format!("{root}/ABOUT"),
                SimpleFileOptions::default().unix_permissions(0o644),
            )
            .unwrap();
            std::io::Write::write_all(&mut zip, b"chrome for testing\n").unwrap();
            zip.start_file(
                format!("{root}/{}", platform.binary_name()),
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
            std::io::Write::write_all(&mut zip, binary).unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// A "browser" that answers `--version`, so the install path's acceptance
    /// check can run without a 100 MB download.
    fn fake_browser(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho '{version}'\n").into_bytes()
    }

    struct FakeSource {
        manifest: String,
        archive: Vec<u8>,
        md5: Option<String>,
        /// Every URL `download` was asked for, so a test can assert the
        /// install did not silently re-fetch.
        fetched: std::sync::Mutex<Vec<String>>,
    }

    impl FakeSource {
        fn new(archive: Vec<u8>) -> Self {
            let md5 = base64::engine::general_purpose::STANDARD
                .encode(Md5::digest(&archive))
                .to_string();
            Self {
                manifest: MANIFEST.to_string(),
                archive,
                md5: Some(md5),
                fetched: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AssetSource for FakeSource {
        async fn manifest(&self) -> Result<String, String> {
            Ok(self.manifest.clone())
        }

        async fn download(
            &self,
            url: &str,
            dest: &Path,
            progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
        ) -> Result<Downloaded, String> {
            self.fetched.lock().unwrap().push(url.to_string());
            let total = self.archive.len() as u64;
            progress(0, Some(total));
            std::fs::write(dest, &self.archive).map_err(|e| e.to_string())?;
            progress(total, Some(total));
            Ok(Downloaded {
                bytes: total,
                md5_base64: self.md5.clone(),
            })
        }
    }

    fn platform_or_skip() -> Option<CftPlatform> {
        CftPlatform::detect()
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn installs_verifies_and_reports_the_browser_version() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        let mut out = Vec::new();

        let installed = provision_chromium(&source, root.path(), &mut out)
            .await
            .expect("the fixture archive installs");

        assert_eq!(installed.version, "151.0.7922.76");
        assert!(!installed.reused);
        assert_eq!(installed.integrity, Some(Integrity::Md5Verified));
        // The acceptance criterion: the thing on disk actually ran.
        assert_eq!(installed.reported_version, "Fake Chrome 151.0");
        assert!(installed.binary.is_file(), "{}", installed.binary.display());
        // The install is versioned, directly under the runtime root — that is
        // what keeps the next upgrade from writing over this one.
        let dir = installed.binary.parent().unwrap();
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            version_dir_name("151.0.7922.76", platform)
        );
        assert_eq!(
            dir.parent(),
            Some(root.path().join(INSTALL_SUBDIR).as_path())
        );

        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("151.0.7922.76"), "{printed}");
    }

    /// Re-running setup must not re-download 100 MB, and must not disturb a
    /// working install.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_second_run_reuses_the_existing_install() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        let first = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap();

        let second = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap();

        assert!(second.reused);
        assert_eq!(second.binary, first.binary);
        assert_eq!(second.integrity, None, "nothing was fetched to verify");
        assert_eq!(source.fetched.lock().unwrap().len(), 1);
    }

    /// The half-extracted-directory failure: an install that exists but does
    /// not run is replaced rather than trusted.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_broken_existing_install_is_replaced_not_trusted() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let dir = root
            .path()
            .join(INSTALL_SUBDIR)
            .join(version_dir_name("151.0.7922.76", platform));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(platform.binary_name()), b"truncated garbage").unwrap();

        let source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        let installed = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap();

        assert!(!installed.reused);
        assert_eq!(installed.reported_version, "Fake Chrome 151.0");
    }

    /// A browser that cannot run must not end up at the path that means
    /// "installed" — otherwise the next run reuses it and `web_search` is
    /// broken with no way to notice.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_archive_whose_binary_does_not_run_installs_nothing() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        // Valid zip, valid layout, contents that are not an executable.
        let source = FakeSource::new(fixture_archive(platform, b"\x7fELF-but-not-really"));

        let err = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap_err();

        assert!(err.contains("did not run"), "{err}");
        assert!(
            err.contains("libnss3"),
            "the remedy must be in the message: {err}"
        );
        let dir = root
            .path()
            .join(INSTALL_SUBDIR)
            .join(version_dir_name("151.0.7922.76", platform));
        assert!(!dir.exists(), "a failed install must leave nothing behind");
    }

    /// The archive contains the right binary under the wrong root — the shape
    /// a Chrome for Testing layout change would take.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_unexpected_archive_layout_fails_loudly_and_installs_nothing() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            use zip::write::SimpleFileOptions;
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("somewhere-else/chrome", SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zip, b"x").unwrap();
            zip.finish().unwrap();
        }
        let source = FakeSource::new(buf.into_inner());

        let err = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap_err();

        assert!(err.contains(&platform.archive_root()), "{err}");
        assert!(err.contains("layout"), "{err}");
        let staging: Vec<_> = std::fs::read_dir(root.path().join(INSTALL_SUBDIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(STAGING_PREFIX))
            .collect();
        assert!(staging.is_empty(), "left staging behind: {staging:?}");
    }

    /// Corruption between the server and the disk. The partial file must go,
    /// or every retry resumes into the same bad bytes.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_checksum_mismatch_refuses_the_archive_and_discards_it() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        source.md5 = Some("AAAAAAAAAAAAAAAAAAAAAA==".to_string());

        let err = provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap_err();

        assert!(err.contains("checksum"), "{err}");
        assert!(err.contains("smith setup"), "remedy missing: {err}");
        let downloads = root.path().join(INSTALL_SUBDIR).join(DOWNLOADS_SUBDIR);
        let leftovers: Vec<_> = std::fs::read_dir(&downloads)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "kept bad bytes: {leftovers:?}");
    }

    /// No checksum published is a legitimate outcome, and must install — but
    /// it must also be reported as what it is rather than as "verified".
    #[tokio::test]
    #[cfg(unix)]
    async fn an_archive_with_no_published_checksum_installs_and_says_so() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        source.md5 = None;
        let mut out = Vec::new();

        let installed = provision_chromium(&source, root.path(), &mut out)
            .await
            .unwrap();

        assert_eq!(installed.integrity, Some(Integrity::LengthOnly));
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("no checksum"), "{printed}");
    }

    /// A crashed earlier run leaves a `.staging-*` directory. It must not
    /// accumulate, and it must not be mistaken for an install.
    #[tokio::test]
    #[cfg(unix)]
    async fn stale_staging_from_a_crashed_run_is_swept() {
        let Some(platform) = platform_or_skip() else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let install_root = root.path().join(INSTALL_SUBDIR);
        let stale = install_root.join(format!("{STAGING_PREFIX}999999"));
        std::fs::create_dir_all(stale.join("junk")).unwrap();

        let source = FakeSource::new(fixture_archive(
            platform,
            &fake_browser("Fake Chrome 151.0"),
        ));
        provision_chromium(&source, root.path(), &mut Vec::new())
            .await
            .unwrap();

        assert!(!stale.exists(), "stale staging survived");
    }

    #[test]
    fn a_hostile_zip_entry_is_refused_before_it_is_written() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("evil.zip");
        {
            use zip::write::SimpleFileOptions;
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            zip.start_file("../escaped", SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zip, b"pwned").unwrap();
            zip.finish().unwrap();
        }
        let into = root.path().join("into");

        let err = extract_zip(&archive, &into, CftPlatform::Linux64).unwrap_err();

        assert!(err.contains("outside"), "{err}");
        assert!(!root.path().join("escaped").exists());
    }

    #[tokio::test]
    async fn probing_a_missing_binary_is_an_error_not_a_panic() {
        let err = probe_version(Path::new("/nonexistent/chrome"))
            .await
            .unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    /// Opt-in, because it downloads ~100 MB and hits the real endpoint:
    ///
    /// ```sh
    /// cargo test -p smith-cli --bin smith runtime -- --ignored --nocapture
    /// ```
    ///
    /// This is what catches Chrome for Testing changing its manifest shape,
    /// its archive layout or its asset names — none of which a fixture can.
    #[tokio::test]
    #[ignore = "downloads ~100 MB from the real Chrome for Testing endpoint"]
    async fn live_provisioning_installs_a_runnable_browser() {
        let root = tempfile::tempdir().unwrap();
        let source = HttpAssetSource::new().unwrap();
        let installed = provision_chromium(&source, root.path(), &mut std::io::stdout())
            .await
            .expect("provisioning should succeed");
        println!(
            "installed {} at {}\n  reported: {}\n  integrity: {:?}",
            installed.version,
            installed.binary.display(),
            installed.reported_version,
            installed.integrity
        );
        assert!(installed.reported_version.contains("Chrome"));
        assert_eq!(installed.integrity, Some(Integrity::Md5Verified));
    }

    /// Opt-in for the same reason as above. Pre-seeds a truncated archive and
    /// checks the transfer picks up from there rather than starting over —
    /// the one behaviour a fixture source cannot exercise, because it depends
    /// on the server honouring `Range`.
    ///
    /// ```sh
    /// cargo test -p smith-cli --bin smith live_resume -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "downloads from the real Chrome for Testing endpoint"]
    async fn live_resume_continues_a_partial_download() {
        let platform = CftPlatform::detect().expect("this platform has a published build");
        let source = HttpAssetSource::new().unwrap();
        let build = parse_manifest(&source.manifest().await.unwrap(), platform).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial.zip");
        // A prefix of the real archive, obtained the same way a killed
        // download would have left one.
        let seeded = 4 * 1024 * 1024u64;
        {
            let head = reqwest::Client::new()
                .get(&build.url)
                .header("range", format!("bytes=0-{}", seeded - 1))
                .send()
                .await
                .unwrap();
            let bytes = head.bytes().await.unwrap();
            assert_eq!(bytes.len() as u64, seeded, "server ignored the seed range");
            std::fs::write(&dest, &bytes).unwrap();
        }

        let mut first_report = None;
        let downloaded = source
            .download(&build.url, &dest, &mut |done, _| {
                first_report.get_or_insert(done);
            })
            .await
            .unwrap();

        assert_eq!(
            first_report,
            Some(seeded),
            "the transfer restarted from zero instead of resuming"
        );
        let on_disk = std::fs::metadata(&dest).unwrap().len();
        assert_eq!(on_disk, downloaded.bytes);
        // The proof the two halves were stitched together correctly.
        assert_eq!(
            verify_archive(&dest, downloaded.bytes, downloaded.md5_base64.as_deref()).unwrap(),
            Integrity::Md5Verified
        );
    }
}
