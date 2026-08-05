//! The handful of headless behaviours that only exist at the process
//! boundary, and are therefore the one justified exception to this
//! repository's inline-tests rule.
//!
//! Whether stdin is a pipe, whether stdout is a terminal, and what the process
//! exits with are properties of file descriptors the test harness owns and
//! cannot hand over. An in-process test can call `read_piped_stdin`, but it
//! reads *cargo's* stdin, which proves nothing about `cat x | smith`. So these
//! spawn the real binary.
//!
//! Every case here stops before a provider request is ever made: `HOME` and
//! `--cwd` point at empty temporary directories and the API-key variables are
//! stripped, so `build_provider` fails and the run exits 2. That failure is
//! the assertion tool — reaching it proves the whole path in front of it
//! (stdin read, prompt composed, headless frontend chosen over the TUI) ran,
//! and it guarantees the suite never touches the network.

use std::io::Write;
use std::process::{Command, Stdio};

const EXIT_USAGE: i32 = 2;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the real `smith` binary with a hermetic environment and `input` on
/// stdin. stdout and stderr are pipes, which is also what makes this a no-TTY
/// run.
fn smith(args: &[&str], input: &str) -> (Run, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_smith"))
        .args(["--cwd", &project.path().to_string_lossy()])
        .args(args)
        .env("HOME", home.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    (
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        project,
    )
}

/// `cat error.log | smith -p "diagnose this"`, minus the provider.
#[test]
fn a_piped_prompt_is_read_and_the_run_gets_as_far_as_the_provider() {
    let (run, _project) = smith(&["-p", "diagnose this"], "thread 'main' panicked\n");

    assert_eq!(run.code, EXIT_USAGE, "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("No Anthropic API key"),
        "stderr: {}",
        run.stderr
    );
}

/// Piping alone, with no `-p`, is enough: stdin becomes the prompt. The
/// contrast with the empty-stdin case below is what proves the bytes were
/// actually read rather than the flag defaulting somewhere.
#[test]
fn stdin_alone_supplies_the_prompt() {
    let (run, _project) = smith(&[], "explain this repository\n");

    assert_eq!(run.code, EXIT_USAGE, "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("No Anthropic API key"),
        "stderr: {}",
        run.stderr
    );
}

/// Same invocation, nothing piped: the run stops one step earlier, at "there
/// is no prompt", instead of blocking on a terminal that isn't there.
#[test]
fn an_empty_pipe_and_no_flag_is_a_usage_error_not_a_hang() {
    let (run, _project) = smith(&[], "");

    assert_eq!(run.code, EXIT_USAGE, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("no prompt"), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "stdout: {:?}", run.stdout);
}

/// The TUI must never start when stdout is a pipe. It would emit terminal
/// escape sequences into the pipe and then wait forever for a keypress that
/// cannot arrive — a CI job that hangs until its timeout.
#[test]
fn a_non_terminal_stdout_never_starts_the_tui() {
    let (run, _project) = smith(&[], "hello");

    // The alternate-screen switch is the TUI's first act; a single escape
    // byte anywhere on stdout means it started.
    assert!(!run.stdout.contains('\x1b'), "stdout: {:?}", run.stdout);
    assert!(!run.stdout.contains("smith"), "stdout: {:?}", run.stdout);
}

/// `--cwd` has to take effect before anything derives a path from the working
/// directory — the session store is the visible proof, since it creates
/// `.smith/` in whichever directory the run decided it was about.
#[test]
fn cwd_moves_the_project_directory_the_run_operates_on() {
    let (_run, project) = smith(&["-p", "hi"], "");

    assert!(
        project.path().join(".smith").is_dir(),
        "the run did not treat --cwd as its project directory"
    );
}

/// A bad `--cwd` is caught before anything else happens, and reported as the
/// usage error it is rather than as a mysterious failure later on.
#[test]
fn an_unusable_cwd_is_rejected_up_front() {
    let output = Command::new(env!("CARGO_BIN_EXE_smith"))
        .args(["--cwd", "/definitely/not/a/directory", "-p", "hi"])
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--cwd"));
}
