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
        ..Default::default()
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
#[cfg(unix)]
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
#[cfg(unix)]
fn fake_browser(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho '{version}'\n").into_bytes()
}

#[cfg(unix)]
struct FakeSource {
    manifest: String,
    archive: Vec<u8>,
    md5: Option<String>,
    /// Every URL `download` was asked for, so a test can assert the
    /// install did not silently re-fetch.
    fetched: std::sync::Mutex<Vec<String>>,
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

/// `ETXTBSY` is a race against an unrelated `fork`, so it must be retried
/// rather than reported as a broken download.
#[test]
fn the_text_file_busy_race_is_recognised_by_errno_not_by_wording() {
    assert!(is_text_file_busy(
        "could not run /tmp/x: Text file busy (os error 26)"
    ));
    // The same errno with a localised message still has to be caught.
    assert!(is_text_file_busy(
        "could not run /tmp/x: Arquivo de texto ocupado (os error 26)"
    ));
    // A genuinely broken binary must not be retried forever.
    assert!(!is_text_file_busy(
        "could not run /tmp/x: Exec format error (os error 8)"
    ));
    assert!(!is_text_file_busy("--version exited with exit status: 127"));
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
