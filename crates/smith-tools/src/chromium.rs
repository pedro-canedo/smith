//! Headless-Chromium search backend for `web_search`.
//!
//! A browser the user already has installed is a search backend nobody has to
//! pay for: it renders the page the way a real visitor would, so the results
//! are the ones a human would see rather than whatever a stripped-down HTML
//! endpoint decides to serve a bare HTTP client. That makes this the tier that
//! keeps `web_search` useful with no API key configured at all.
//!
//! Everything here is best effort. A machine with no Chromium, a browser that
//! refuses to start, a results page that changed shape — each returns `Err`
//! and lets `web_search` fall through to the next tier. Nothing in this module
//! is allowed to be the reason a turn fails.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::web_search::{
    decode_html_entities, resolve_duckduckgo_redirect, strip_tags, SearchResult,
};

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

/// The results page. The `html.` host is DuckDuckGo's no-JavaScript rendering
/// of the normal search page: same results, markup a parser can survive.
const SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

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
const KILL_GRACE: Duration = Duration::from_millis(200);

/// How long a browser that has finished writing gets to exit before it is
/// killed instead of waited on.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Markers for the two pieces of each result row in DuckDuckGo's HTML output.
const RESULT_LINK_CLASS: &str = "result__a";
const RESULT_SNIPPET_CLASS: &str = "result__snippet";

/// DuckDuckGo routes sponsored links through this instead of its usual
/// `/l/?uddg=` redirect, which is what makes ads separable from results
/// without a real HTML parser.
const AD_REDIRECT_MARKER: &str = "duckduckgo.com/y.js";

/// Whether a browser could be found at all. Cheap to call repeatedly — the
/// filesystem probe happens once per process.
pub(crate) fn is_available() -> bool {
    browser_path().is_some()
}

/// Runs `query` through a headless browser and returns up to `limit` results.
///
/// `Err` carries a short reason for the log/next tier, never for the model:
/// the caller's job on failure is to try another backend, not to explain this
/// one.
pub(crate) async fn search(
    query: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<SearchResult>, String> {
    let browser = browser_path().ok_or("no Chromium-family browser found")?;
    let url = search_url(query)?;
    let dom = dump_dom(browser, &url, cancel).await?;
    Ok(parse_results(&dom, limit))
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

/// The search URL for `query`, with the query string escaped by the `url`
/// crate rather than by hand — a query with `&` or `#` in it would otherwise
/// silently search for something else.
fn search_url(query: &str) -> Result<String, String> {
    url::Url::parse_with_params(SEARCH_ENDPOINT, &[("q", query)])
        .map(String::from)
        .map_err(|e| format!("could not build a search URL: {e}"))
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

/// Pulls result rows out of DuckDuckGo's HTML output.
///
/// Deliberately a scanner and not an HTML parser: the only structure that
/// matters is "a `result__a` anchor, then its `result__snippet`", and matching
/// on those two class names degrades to *fewer results* when the markup
/// shifts, rather than to a wrong answer. An empty return is a normal outcome
/// (a challenge page, a zero-result query) and sends `web_search` to its next
/// tier.
fn parse_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;

    while results.len() < limit {
        let Some(marker) = rest.find(RESULT_LINK_CLASS) else {
            break;
        };
        // The class attribute sits inside the anchor tag, so the tag itself
        // starts at the nearest `<a` before it.
        let Some(tag_start) = rest[..marker].rfind("<a") else {
            rest = &rest[marker + RESULT_LINK_CLASS.len()..];
            continue;
        };
        let Some(tag_end) = rest[marker..].find('>').map(|i| marker + i) else {
            break;
        };
        let Some(text_end) = rest[tag_end..].find("</a>").map(|i| tag_end + i) else {
            break;
        };

        let href =
            decode_html_entities(attr(&rest[tag_start..tag_end], "href").unwrap_or_default());
        let title = strip_tags(&rest[tag_end + 1..text_end]);
        let snippet = snippet_after(&rest[text_end..]);

        rest = &rest[text_end + "</a>".len()..];

        // Sponsored rows use the same result markup; they are the one thing
        // here worth dropping outright rather than passing to the model as if
        // it were an organic result.
        if href.contains(AD_REDIRECT_MARKER) || title.is_empty() {
            continue;
        }
        if let Some(url) = resolve_duckduckgo_redirect(&href) {
            results.push(SearchResult {
                title,
                url,
                snippet,
                // The HTML endpoint carries no publication dates, so this tier
                // never contributes a recency signal — same as the lite one.
                published: None,
            });
        }
    }

    results
}

/// The snippet belonging to the result that `rest` starts at — i.e. one that
/// appears before the *next* result link. Without that bound, a result with no
/// snippet of its own would borrow the following result's.
fn snippet_after(rest: &str) -> String {
    let Some(start) = rest.find(RESULT_SNIPPET_CLASS) else {
        return String::new();
    };
    if let Some(next_result) = rest.find(RESULT_LINK_CLASS) {
        if next_result < start {
            return String::new();
        }
    }
    let Some(open) = rest[start..].find('>').map(|i| start + i + 1) else {
        return String::new();
    };
    // `</a>` on the HTML endpoint, `</td>` on the lite one — take whichever
    // closes first so the same scanner survives either.
    let end = ["</a>", "</td>"]
        .iter()
        .filter_map(|close| rest[open..].find(close))
        .min()
        .map(|i| open + i)
        .unwrap_or(rest.len());
    strip_tags(&rest[open..end])
}

/// Reads a double-quoted attribute out of a single tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One organic result row in the shape DuckDuckGo's HTML endpoint emits.
    fn result_row(n: u32, title: &str, snippet: &str) -> String {
        format!(
            r#"<div class="result results_links">
                 <h2 class="result__title">
                   <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F{n}&amp;rut=abc">{title}</a>
                 </h2>
                 <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">{snippet}</a>
               </div>"#
        )
    }

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
    fn search_url_escapes_the_query() {
        let url = search_url("rust & c++ #1").unwrap();
        assert!(url.starts_with(SEARCH_ENDPOINT), "got: {url}");
        // The `&` must not be able to introduce a second query parameter.
        assert!(url.contains("q=rust+%26+c%2B%2B+%231"), "got: {url}");
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

    #[test]
    fn parses_title_url_and_snippet_from_a_result_row() {
        let html = result_row(1, "Example Page", "A short summary of the page.");
        let results = parse_results(&html, 3);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Page");
        assert_eq!(results[0].url, "https://example.com/1");
        assert_eq!(results[0].snippet, "A short summary of the page.");
        assert_eq!(results[0].published, None);
    }

    /// The acceptance criterion the whole tier exists for: the top three
    /// results, each with a title, a URL and a summary.
    #[test]
    fn returns_the_top_three_results_in_page_order() {
        let html = (1..=6)
            .map(|n| result_row(n, &format!("Title {n}"), &format!("Summary {n}")))
            .collect::<Vec<_>>()
            .join("\n");
        let results = parse_results(&html, 3);
        assert_eq!(results.len(), 3);
        for (i, r) in results.iter().enumerate() {
            let n = i + 1;
            assert_eq!(r.title, format!("Title {n}"));
            assert_eq!(r.url, format!("https://example.com/{n}"));
            assert_eq!(r.snippet, format!("Summary {n}"));
        }
    }

    #[test]
    fn strips_markup_and_entities_out_of_titles_and_snippets() {
        let html = result_row(1, "Rust &amp; <b>C++</b>", "Compared <em>side by side</em>");
        let results = parse_results(&html, 3);
        assert_eq!(results[0].title, "Rust & C++");
        assert_eq!(results[0].snippet, "Compared side by side");
    }

    #[test]
    fn skips_sponsored_rows() {
        let ad = r##"<a rel="nofollow" class="result__a" href="//duckduckgo.com/y.js?ad_provider=x">Buy Rust Now</a>
                     <a class="result__snippet" href="#">Sponsored.</a>"##;
        let html = format!("{ad}\n{}", result_row(2, "Real Result", "Organic."));
        let results = parse_results(&html, 3);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].title, "Real Result");
    }

    /// A result with no snippet of its own must not borrow the next one's —
    /// an attributed-to-the-wrong-page summary is worse than no summary.
    #[test]
    fn a_result_without_a_snippet_does_not_borrow_the_next_one() {
        let html = format!(
            r#"<a rel="nofollow" class="result__a" href="https://first.example/">First</a>
               {}"#,
            result_row(2, "Second", "Second summary")
        );
        let results = parse_results(&html, 3);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].snippet, "");
        assert_eq!(results[1].snippet, "Second summary");
    }

    /// A challenge page, a rate-limit page, or a genuinely empty query all
    /// land here — and all have to be a clean empty list, because that is what
    /// sends `web_search` on to its next backend.
    #[test]
    fn a_page_with_no_results_parses_as_empty() {
        assert!(parse_results("<html><body>no results here</body></html>", 3).is_empty());
        assert!(parse_results("", 3).is_empty());
    }

    /// Truncated markup (a capped read, a browser killed mid-dump) must stop,
    /// not spin or panic on a slice boundary.
    #[test]
    fn truncated_markup_stops_cleanly() {
        let full = result_row(1, "Example Page", "A summary.");
        for cut in 0..full.len() {
            let _ = parse_results(&full[..cut], 3);
        }
    }

    /// The one test that launches a real browser and hits the real network,
    /// so it is opt-in:
    ///
    /// ```sh
    /// cargo test -p smith-tools --lib chromium -- --ignored --nocapture
    /// ```
    ///
    /// Everything above pins the parsing and the argument list against fixed
    /// input; this is what catches the live page changing shape underneath us,
    /// which no fixture can.
    #[tokio::test]
    #[ignore = "launches a real browser and requires network access"]
    async fn live_search_returns_three_usable_results() {
        let Some(browser) = browser_path() else {
            panic!("no Chromium-family browser found — install one or set {BROWSER_PATH_ENV}");
        };
        println!("using {}", browser.display());

        let results = search("rust programming language", 3, &CancellationToken::new())
            .await
            .expect("the search should succeed");

        assert!(!results.is_empty(), "no results parsed off the live page");
        assert!(results.len() <= 3);
        for r in &results {
            println!("{} — {}\n  {}", r.title, r.url, r.snippet);
            assert!(!r.title.is_empty());
            assert!(r.url.starts_with("http"), "not a real URL: {}", r.url);
        }
    }

    #[test]
    fn reads_a_quoted_attribute_out_of_a_tag() {
        let tag = r#"<a rel="nofollow" class="result__a" href="https://example.com/x""#;
        assert_eq!(attr(tag, "href"), Some("https://example.com/x"));
        assert_eq!(attr(tag, "title"), None);
    }
}
