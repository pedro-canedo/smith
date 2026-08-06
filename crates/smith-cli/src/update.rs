//! Release discovery and self-update for the installed `smith` executable.
//!
//! The startup path only performs a cached, short-bounded check and never
//! changes the executable. `smith update` is the explicit mutating operation:
//! it downloads the platform archive, verifies its published SHA-256, and
//! replaces the current executable atomically where the platform allows it.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "pedro-canedo/smith";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckCache {
    checked_at: u64,
    latest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    archive_suffix: &'static str,
    binary_name: &'static str,
}

pub async fn run() -> std::process::ExitCode {
    match update().await {
        Ok(UpdateOutcome::UpToDate) => {
            println!("smith {} is already up to date", current_version());
            std::process::ExitCode::SUCCESS
        }
        Ok(UpdateOutcome::Updated { version }) => {
            println!("updated smith {} -> {}", current_version(), version);
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("smith update: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

pub async fn startup_notice() {
    if std::env::var_os("SMITH_DISABLE_UPDATE_CHECK").is_some() {
        return;
    }
    if let Ok(Ok(Some(version))) = tokio::time::timeout(CHECK_TIMEOUT, check_for_update()).await {
        eprintln!(
            "smith {version} is available (current {}). Run `smith update` to install it.",
            current_version()
        );
    }
}

pub async fn auto_update() {
    match update().await {
        Ok(UpdateOutcome::UpToDate) => {}
        Ok(UpdateOutcome::Updated { version }) => {
            eprintln!("smith automatically updated to {version}");
        }
        Err(error) => {
            eprintln!("smith auto-update: {error}");
        }
    }
}

enum UpdateOutcome {
    UpToDate,
    Updated { version: String },
}

async fn update() -> Result<UpdateOutcome, String> {
    let release = latest_release().await?;
    if !is_newer(&release.tag_name, current_version()) {
        return Ok(UpdateOutcome::UpToDate);
    }

    let target = target().ok_or_else(|| {
        format!(
            "no published binary for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let version = release.tag_name.trim_start_matches('v');
    let archive_name = format!("smith-{version}-{}", target.archive_suffix);
    let checksum_name = format!("{archive_name}.sha256");
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .ok_or_else(|| format!("release is missing {archive_name}"))?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .ok_or_else(|| format!("release is missing {checksum_name}"))?;

    let client = http_client()?;
    let archive_bytes = client
        .get(&archive.browser_download_url)
        .send()
        .await
        .map_err(|e| format!("could not download {archive_name}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("could not download {archive_name}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("could not read {archive_name}: {e}"))?;
    let checksum_text = client
        .get(&checksum.browser_download_url)
        .send()
        .await
        .map_err(|e| format!("could not download {checksum_name}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("could not download {checksum_name}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("could not read {checksum_name}: {e}"))?;
    verify_checksum(&archive_bytes, &checksum_text, &archive_name)?;

    let binary = extract_binary(&archive_bytes, &archive_name, target.binary_name)?;
    install_binary(&binary, target.binary_name)?;
    write_cache(Some(&release.tag_name));
    Ok(UpdateOutcome::Updated {
        version: release.tag_name,
    })
}

async fn check_for_update() -> Result<Option<String>, String> {
    let now = unix_now();
    if let Some(cache) = read_cache() {
        if now.saturating_sub(cache.checked_at) < CHECK_INTERVAL.as_secs() {
            return Ok(cache.latest.filter(|tag| is_newer(tag, current_version())));
        }
    }

    let release = latest_release().await?;
    write_cache(Some(&release.tag_name));
    Ok(is_newer(&release.tag_name, current_version()).then_some(release.tag_name))
}

async fn latest_release() -> Result<Release, String> {
    let response = http_client()?
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .send()
        .await
        .map_err(|e| format!("could not check GitHub Releases: {e}"))?
        .error_for_status()
        .map_err(|e| format!("could not check GitHub Releases: {e}"))?;
    response
        .json()
        .await
        .map_err(|e| format!("GitHub returned an invalid release description: {e}"))
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("smith/{}", current_version()))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("could not create update client: {e}"))
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn target() -> Option<Target> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(Target {
            archive_suffix: "x86_64-unknown-linux-gnu.tar.gz",
            binary_name: "smith",
        }),
        ("linux", "aarch64") => Some(Target {
            archive_suffix: "aarch64-unknown-linux-gnu.tar.gz",
            binary_name: "smith",
        }),
        ("macos", "x86_64") => Some(Target {
            archive_suffix: "x86_64-apple-darwin.tar.gz",
            binary_name: "smith",
        }),
        ("macos", "aarch64") => Some(Target {
            archive_suffix: "aarch64-apple-darwin.tar.gz",
            binary_name: "smith",
        }),
        ("windows", "x86_64") => Some(Target {
            archive_suffix: "x86_64-pc-windows-msvc.zip",
            binary_name: "smith.exe",
        }),
        _ => None,
    }
}

fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(version: &str) -> (u64, u64, u64) {
    let mut parts = version.trim_start_matches('v').split('.');
    (
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
    )
}

fn verify_checksum(bytes: &[u8], checksum: &str, archive_name: &str) -> Result<(), String> {
    let expected = checksum
        .split_whitespace()
        .next()
        .filter(|hash| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("invalid checksum file for {archive_name}"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected.to_ascii_lowercase() {
        return Err(format!("SHA-256 mismatch for {archive_name}"));
    }
    Ok(())
}

fn extract_binary(bytes: &[u8], archive_name: &str, binary_name: &str) -> Result<Vec<u8>, String> {
    if archive_name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| format!("could not open {archive_name}: {e}"))?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| format!("could not read {archive_name}: {e}"))?;
            if entry.name().ends_with(binary_name) {
                let mut binary = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut binary)
                    .map_err(|e| format!("could not extract {binary_name}: {e}"))?;
                return Ok(binary);
            }
        }
    } else {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive
            .entries()
            .map_err(|e| format!("could not open {archive_name}: {e}"))?
        {
            let mut entry = entry.map_err(|e| format!("could not read {archive_name}: {e}"))?;
            let path = entry
                .path()
                .map_err(|e| format!("could not inspect {archive_name}: {e}"))?;
            if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
                let mut binary = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut binary)
                    .map_err(|e| format!("could not extract {binary_name}: {e}"))?;
                return Ok(binary);
            }
        }
    }
    Err(format!("release archive does not contain {binary_name}"))
}

fn install_binary(binary: &[u8], binary_name: &str) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| format!("could not locate smith: {e}"))?;
    let parent = current
        .parent()
        .ok_or_else(|| "smith executable has no parent directory".to_string())?;
    let staged = parent.join(format!(".{binary_name}.update"));
    std::fs::write(&staged, binary).map_err(|e| format!("could not stage update: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&current)
            .map_err(|e| format!("could not inspect smith: {e}"))?
            .permissions()
            .mode();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("could not preserve smith permissions: {e}"))?;
        std::fs::rename(&staged, &current).map_err(|e| format!("could not install update: {e}"))?;
    }

    #[cfg(windows)]
    {
        let current = current
            .to_str()
            .ok_or_else(|| "smith path is not valid Unicode".to_string())?;
        let staged = staged
            .to_str()
            .ok_or_else(|| "staged update path is not valid Unicode".to_string())?;
        let script = format!(
            "Start-Sleep -Milliseconds 500; Move-Item -LiteralPath '{}' -Destination '{}' -Force",
            powershell_quote(staged),
            powershell_quote(current),
        );
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(script)
            .spawn()
            .map_err(|e| format!("could not schedule Windows update: {e}"))?;
    }

    Ok(())
}

#[cfg(windows)]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn cache_path() -> Option<PathBuf> {
    smith_config::config_dir()
        .ok()
        .map(|dir| dir.join("update-check.json"))
}

fn read_cache() -> Option<CheckCache> {
    let path = cache_path()?;
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_cache(latest: Option<&str>) {
    let Some(path) = cache_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let cache = CheckCache {
        checked_at: unix_now(),
        latest: latest.map(str::to_string),
    };
    let _ = std::fs::write(path, serde_json::to_vec(&cache).unwrap_or_default());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_ignores_the_tag_prefix() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }

    #[test]
    fn checksum_accepts_github_checksum_format() {
        let bytes = b"smith";
        let checksum = format!(
            "{:x}  smith-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n",
            Sha256::digest(bytes)
        );
        assert!(verify_checksum(bytes, &checksum, "archive.tar.gz").is_ok());
    }

    #[test]
    fn checksum_rejects_wrong_bytes() {
        let checksum = format!("{:x}  archive.tar.gz", Sha256::digest(b"other"));
        assert!(verify_checksum(b"smith", &checksum, "archive.tar.gz").is_err());
    }
}
