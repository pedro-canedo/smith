//! Headless-Chromium page fetcher, used by `web_search` as a second network
//! path to the same search endpoint.
//!
//! This module drives a browser and hands back the rendered DOM; it knows
//! nothing about search engines or result markup. That split is deliberate.
//! It used to scrape DuckDuckGo's HTML endpoint directly, which stopped
//! working — measured, that endpoint now answers a 14 KB challenge page to a
//! *real headless browser* just as it does to a bare HTTP client, so rendering
//! it bought nothing. What a browser is still uniquely good for is being a
//! different client: its own TLS stack and HTTP/2 fingerprint get through
//! interception that a plain request does not. So the caller picks the URL
//! (today, Bing's RSS feed — see [`crate::bing`]) and owns the parsing, and
//! this module just fetches.
//!
//! Everything here is best effort. A machine with no Chromium, a browser that
//! refuses to start, a page that never finishes — each returns `Err` and lets
//! `web_search` fall through to the next tier. Nothing in this module is
//! allowed to be the reason a turn fails.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

/// Set this to point at a specific browser binary. Honoured verbatim — an
/// explicit setting that silently fell back to some other browser would be
/// worse than a clear spawn failure.
const BROWSER_PATH_ENV: &str = "SMITH_CHROMIUM_PATH";
/// The de-facto standard name for the same thing, shared with other tools;
/// checked second so smith's own variable always wins.
const BROWSER_PATH_ENV_FALLBACK: &str = "CHROME_PATH";

/// Probed in order, first hit wins. Names go through `PATH`; the absolute
/// paths at the end are the macOS bundles, which are never on `PATH`.
const BROWSER_CANDIDATES: &[&str] = &[
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

/// A cold browser start plus a page load is seconds, not milliseconds, so this
/// is generous — but it is still a hard ceiling, because the failure mode it
/// guards against is a browser that never exits at all and hangs the turn
/// behind it.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the page gets to settle before Chromium dumps the DOM. Virtual
/// time, not wall clock: Chromium fast-forwards its own timers, so this costs
/// far less than eight real seconds on a page that finishes early.
const VIRTUAL_TIME_BUDGET_MS: u64 = 8_000;

/// Bounds the DOM we're willing to read back. A results page is ~100 KB; this
/// is room for a pathological one without letting a runaway browser fill
/// memory.
const MAX_DOM_BYTES: usize = 2 * 1024 * 1024;

/// Sent instead of Chromium's own headless UA, which some sites answer with a
/// stripped or challenge page.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// How long a browser being torn down gets to exit on its own.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

/// How long a browser that has finished writing gets to exit before it is
/// killed instead of waited on.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Whether a browser could be found at all. Cheap to call repeatedly — the
/// filesystem probe happens once per process.
pub(crate) fn is_available() -> bool {
    browser_path().is_some()
}

/// Loads `url` in a headless browser and returns the rendered DOM.
///
/// `Err` carries a short reason for the next tier: the caller's job on failure
/// is to try another backend, not to explain this one.
pub(crate) async fn fetch(url: &str, cancel: &CancellationToken) -> Result<String, String> {
    let browser = browser_path().ok_or("no Chromium-family browser found")?;
    dump_dom(browser, url, cancel).await
}

/// Where `smith setup` installs the browser it provisions. Probed directly
/// rather than being handed over in an environment variable: exporting one
/// from `main` meant the browser was only findable when the process happened
/// to have gone through that code path, so a differently-wired frontend — or
/// a test — would silently see no browser at all.
///
/// Versioned, so the newest install wins and an upgrade is never an
/// overwrite. `chrome-headless-shell` rather than full Chrome, deliberately:
/// full Chrome never terminates under the `--dump-dom` invocation below.
const RUNTIME_SUBDIR: &str = "runtime/chrome-headless-shell";
const RUNTIME_BINARY: &str = if cfg!(windows) {
    "chrome-headless-shell.exe"
} else {
    "chrome-headless-shell"
};

/// The newest provisioned browser under `~/.smith`, if any.
fn provisioned_browser() -> Option<PathBuf> {
    let root = directories::BaseDirs::new()?
        .home_dir()
        .join(".smith")
        .join(RUNTIME_SUBDIR);

    let mut installs: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join(RUNTIME_BINARY).is_file())
        .collect();

    // Lexicographic on the versioned directory name is not true semver
    // ordering, but installs are pruned to a handful and any of them works —
    // picking the wrong one costs nothing.
    installs.sort();
    installs.pop().map(|dir| dir.join(RUNTIME_BINARY))
}

/// The resolved browser, probed once per process. `PATH` doesn't change under
/// a running program, so re-walking it on every search would buy nothing.
fn browser_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        // An explicit override always wins — someone pointing at a specific
        // binary means it, even when smith has provisioned one of its own.
        let explicit = std::env::var(BROWSER_PATH_ENV)
            .or_else(|_| std::env::var(BROWSER_PATH_ENV_FALLBACK))
            .ok();
        if let Some(found) = pick_browser(explicit, BROWSER_CANDIDATES, resolve_in_path) {
            return Some(found);
        }
        provisioned_browser()
    })
    .as_deref()
}

/// Browser selection as a pure function: an explicit override wins outright,
/// otherwise the first candidate `resolve` can find. Split out from
/// `browser_path` so the precedence is testable without a real browser on the
/// machine running the tests.
fn pick_browser(
    explicit: Option<String>,
    candidates: &[&str],
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
        return Some(PathBuf::from(explicit));
    }
    candidates.iter().find_map(|c| resolve(c))
}

/// Resolves a bare command name against `PATH`; a name with a separator in it
/// is already a path and is only checked for existence.
fn resolve_in_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The full argument list for one throwaway page load.
///
/// `--user-data-dir` is the load-bearing one: without it Chromium reuses the
/// user's real profile, which both fails outright while their browser is open
/// (the profile is locked) and would mean searching as them, with their
/// cookies. Every launch gets its own directory, and it is deleted afterwards.
fn chromium_args(url: &str, profile: &Path, needs_no_sandbox: bool) -> Vec<String> {
    let mut args = vec![
        "--headless".to_string(),
        "--disable-gpu".to_string(),
        // /dev/shm is tiny in most containers; without this Chromium crashes
        // there rather than falling back to a temp file.
        "--disable-dev-shm-usage".to_string(),
        "--disable-extensions".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--window-size=1280,1024".to_string(),
        format!("--user-data-dir={}", profile.display()),
        format!("--virtual-time-budget={VIRTUAL_TIME_BUDGET_MS}"),
        format!("--user-agent={USER_AGENT}"),
    ];
    if needs_no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    // `--dump-dom` is what makes this a fetch rather than a browsing session:
    // Chromium prints the rendered DOM to stdout and exits. It stays last,
    // immediately before the URL, because that is the documented ordering.
    args.push("--dump-dom".to_string());
    args.push(url.to_string());
    args
}

/// Chromium's own sandbox needs kernel features that are unavailable to a
/// process running as root in a typical container, and it refuses to start
/// rather than run unsandboxed. Disabling it only in that case keeps the
/// sandbox — which is what contains the untrusted page being loaded — for
/// every normal user.
fn needs_no_sandbox() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` reads a process property and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Launches the browser, reads the dumped DOM, and cleans up the throwaway
/// profile on every exit path.
async fn dump_dom(browser: &Path, url: &str, cancel: &CancellationToken) -> Result<String, String> {
    let profile = temp_profile_dir();
    let mut cmd = tokio::process::Command::new(browser);
    cmd.args(chromium_args(url, &profile, needs_no_sandbox()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Chromium is chatty on stderr even on a clean run (GPU, dbus, font
        // warnings) and none of it is diagnostic here.
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // A browser is a process tree — zygote, renderers, GPU process. Its own
    // group is what makes cancelling it kill all of them.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            remove_profile(&profile);
            return Err(format!("could not launch {}: {e}", browser.display()));
        }
    };

    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            kill(&mut child).await;
            Err("cancelled".to_string())
        }
        _ = tokio::time::sleep(LAUNCH_TIMEOUT) => {
            kill(&mut child).await;
            Err(format!("browser did not finish within {}s", LAUNCH_TIMEOUT.as_secs()))
        }
        dom = read_stdout(&mut child) => {
            // The read above is capped, so "stdout is done" does not
            // guarantee the browser is: one that kept writing past the cap is
            // now blocked on a full pipe and would never be reaped. Give it a
            // moment to exit on its own, then take it down.
            if tokio::time::timeout(EXIT_GRACE, child.wait()).await.is_err() {
                kill(&mut child).await;
            }
            dom
        }
    };

    remove_profile(&profile);
    outcome
}

async fn read_stdout(child: &mut Child) -> Result<String, String> {
    let Some(mut stdout) = child.stdout.take() else {
        return Err("browser produced no stdout".to_string());
    };
    let mut buf = Vec::new();
    // Reading a capped prefix rather than to EOF: a browser that keeps writing
    // must not be able to grow this without bound.
    let read = (&mut stdout)
        .take(MAX_DOM_BYTES as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| format!("could not read the page: {e}"))?;
    if read == 0 {
        return Err("browser produced an empty page".to_string());
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Kills the browser and everything it spawned.
#[cfg(unix)]
async fn kill(child: &mut Child) {
    if let Some(pid) = child.id() {
        let pgid = pid as libc::pid_t;
        // SAFETY: signalling a process group this call created; a stale pgid
        // can only produce ESRCH, which is ignored.
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        tokio::time::sleep(KILL_GRACE).await;
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn kill(child: &mut Child) {
    let _ = child.kill().await;
}

/// A directory name unique per launch, without pulling in a random source:
/// the pid separates concurrent smith processes and the counter separates
/// searches within one.
fn temp_profile_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("smith-chromium-{}-{n}", std::process::id()))
}

fn remove_profile(profile: &Path) {
    let _ = std::fs::remove_dir_all(profile);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_browser_path_wins_over_every_candidate() {
        let picked = pick_browser(Some("/opt/my-chrome".to_string()), &["chromium"], |_| {
            Some(PathBuf::from("/usr/bin/chromium"))
        });
        assert_eq!(picked, Some(PathBuf::from("/opt/my-chrome")));
    }

    /// An override pointing at nothing is still honoured: falling back to a
    /// different browser than the one the user named would be a silent lie,
    /// and the spawn error says exactly what went wrong.
    #[test]
    fn explicit_browser_path_is_honoured_even_when_missing() {
        let picked = pick_browser(Some("/nope/chrome".to_string()), &["chromium"], |_| None);
        assert_eq!(picked, Some(PathBuf::from("/nope/chrome")));
    }

    #[test]
    fn blank_override_falls_through_to_the_candidates() {
        let picked = pick_browser(Some("   ".to_string()), &["chromium"], |name| {
            Some(PathBuf::from("/usr/bin").join(name))
        });
        assert_eq!(picked, Some(PathBuf::from("/usr/bin/chromium")));
    }

    #[test]
    fn candidates_are_probed_in_order() {
        let picked = pick_browser(None, &["chromium", "google-chrome"], |name| {
            (name == "google-chrome").then(|| PathBuf::from("/usr/bin/google-chrome"))
        });
        assert_eq!(picked, Some(PathBuf::from("/usr/bin/google-chrome")));
    }

    #[test]
    fn no_browser_anywhere_is_none_rather_than_a_guess() {
        assert_eq!(pick_browser(None, BROWSER_CANDIDATES, |_| None), None);
    }

    #[test]
    fn chromium_args_end_with_dump_dom_then_the_url() {
        let args = chromium_args("https://example.com", Path::new("/tmp/p"), false);
        assert_eq!(args[args.len() - 2], "--dump-dom");
        assert_eq!(args[args.len() - 1], "https://example.com");
    }

    /// The throwaway profile is what keeps a search from touching (or being
    /// blocked by) the user's own browser session.
    #[test]
    fn chromium_args_always_use_a_throwaway_profile() {
        let args = chromium_args("https://example.com", Path::new("/tmp/p"), false);
        assert!(
            args.iter().any(|a| a == "--user-data-dir=/tmp/p"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "--headless"), "{args:?}");
    }

    #[test]
    fn no_sandbox_is_opt_in_rather_than_always_on() {
        let sandboxed = chromium_args("https://example.com", Path::new("/tmp/p"), false);
        assert!(
            !sandboxed.iter().any(|a| a == "--no-sandbox"),
            "{sandboxed:?}"
        );
        let unsandboxed = chromium_args("https://example.com", Path::new("/tmp/p"), true);
        assert!(
            unsandboxed.iter().any(|a| a == "--no-sandbox"),
            "{unsandboxed:?}"
        );
    }

    #[test]
    fn temp_profile_dirs_are_unique_per_launch() {
        assert_ne!(temp_profile_dir(), temp_profile_dir());
    }

    /// The one test that launches a real browser and hits the real network,
    /// so it is opt-in:
    ///
    /// ```sh
    /// cargo test -p smith-tools --lib chromium -- --ignored --nocapture
    /// ```
    ///
    /// Everything above pins the argument list against fixed input; this is
    /// what catches a browser that stopped terminating, or an endpoint that
    /// started refusing one, which no fixture can.
    #[tokio::test]
    #[ignore = "launches a real browser and requires network access"]
    async fn live_fetch_returns_a_parseable_search_feed() {
        let Some(browser) = browser_path() else {
            panic!(
                "no Chromium-family browser found \u{2014} install one or set {BROWSER_PATH_ENV}"
            );
        };
        println!("using {}", browser.display());

        let url = crate::bing::search_url("rust programming language", "en-US").unwrap();
        let dom = fetch(&url, &CancellationToken::new())
            .await
            .expect("the fetch should succeed");

        // Chromium wraps XML in its viewer, but leaves the original markup in
        // the DOM — which is what lets this share a parser with the plain-HTTP
        // tier.
        let results = crate::bing::parse_rss(&dom, 3);
        assert!(!results.is_empty(), "no results parsed off the live feed");
        for r in &results {
            println!("{} \u{2014} {}\n  {}", r.title, r.url, r.snippet);
            assert!(!r.title.is_empty());
            assert!(r.url.starts_with("http"), "not a real URL: {}", r.url);
        }
    }
}
