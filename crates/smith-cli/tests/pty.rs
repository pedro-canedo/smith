#![cfg(all(unix, debug_assertions))]
//! Acceptance criterion #9: a panic must leave the terminal usable.
//!
//! # Why this needs a real pseudo-terminal
//!
//! Everything smith does to the terminal on the way in — raw mode, the
//! alternate screen, bracketed paste, mouse capture, the keyboard enhancement
//! stack — is an `ioctl` or an escape sequence aimed at a tty. With stdout
//! redirected to a pipe, `enable_raw_mode` fails, `supports_keyboard_enhancement`
//! answers false, and the whole restore path is never exercised. A test that
//! captured output through a pipe would pass while the real failure — a shell
//! left in raw mode with no cursor after a crash — went undetected.
//!
//! So the binary is run under a PTY, made to panic at the exact point where it
//! has entered raw mode and the alternate screen, and the bytes it wrote on the
//! way out are inspected for the sequences that undo each of those.
//!
//! `--panic-now` is `cfg(debug_assertions)` and hidden, so this apparatus does
//! not exist in a release build.
//!
//! Like `headless_cli.rs` next door, this is one of the two justified
//! exceptions to the repository's inline-tests rule: the property under test
//! belongs to the process and its terminal, not to any function inside the
//! crate.

/// Alternate screen off (`CSI ? 1049 l`).
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";
/// Cursor visible (`CSI ? 25 h`). A crash that skipped this leaves an
/// invisible caret in the user's shell — the most common way this goes wrong,
/// and the hardest for a user to diagnose.
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Bracketed paste off (`CSI ? 2004 l`). Left on, every paste into the shell
/// afterwards arrives wrapped in `200~`/`201~`.
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
/// Mouse capture off — crossterm emits the whole family (1000/1002/1003/1015/
/// 1006). Left on, the shell fills with escape codes whenever the mouse moves.
const DISABLE_MOUSE_ANY: &str = "\x1b[?1006l";

use std::io::Read;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Runs the smith binary under a PTY with the given arguments and returns
/// everything it wrote, having waited for it to exit.
fn run_under_pty(args: &[&str]) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");

    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_smith"));
    for arg in args {
        command.arg(arg);
    }
    // A predictable terminal: `supports_keyboard_enhancement` queries the
    // terminal and waits for an answer, and the answer decides whether the
    // restore path has a `PopKeyboardEnhancementFlags` to emit.
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    // Otherwise the child inherits the test runner's directory, and a
    // stray `.smith/` in the repo is a side effect of running tests.
    command.cwd(std::env::temp_dir());

    let mut child = pty.slave.spawn_command(command).expect("spawn smith");
    // The slave must be dropped before reading to EOF, or the master never
    // sees the hangup and the read below blocks forever.
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("clone the reader");
    let output = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                panic!("smith did not exit within 30s");
            }
            Err(e) => panic!("waiting for smith: {e}"),
        }
    }
    drop(pty.master);
    output.join().expect("reader thread")
}

/// The criterion itself.
#[test]
fn a_panic_after_terminal_init_restores_every_mode_it_turned_on() {
    let out = run_under_pty(&["--panic-now"]);

    assert!(
        out.contains(LEAVE_ALT_SCREEN),
        "the alternate screen was never left; the user's scrollback is gone.\n{out:?}"
    );
    assert!(
        out.contains(SHOW_CURSOR),
        "the cursor was left hidden.\n{out:?}"
    );
    assert!(
        out.contains(DISABLE_BRACKETED_PASTE),
        "bracketed paste was left on; every later paste arrives wrapped in 200~.\n{out:?}"
    );
    assert!(
        out.contains(DISABLE_MOUSE_ANY),
        "mouse capture was left on; the shell will fill with escape codes.\n{out:?}"
    );
}

/// The panic message still has to reach the user. A restore path that
/// swallowed it would leave a clean terminal and no explanation, which is
/// its own kind of broken.
#[test]
fn the_panic_is_still_reported_after_the_terminal_is_restored() {
    let out = run_under_pty(&["--panic-now"]);
    assert!(
        out.contains("--panic-now"),
        "the panic message did not survive the restore.\n{out:?}"
    );
}

/// The ordering is load-bearing and not visible from the code alone: the
/// restore has to happen *before* the panic message, or the message is
/// written into the alternate screen and vanishes with it.
#[test]
fn the_terminal_is_restored_before_the_panic_is_printed() {
    let out = run_under_pty(&["--panic-now"]);
    let restored = out.find(LEAVE_ALT_SCREEN).expect("alt screen left");
    let reported = out.find("--panic-now").expect("panic reported");
    assert!(
        restored < reported,
        "the panic was printed into the alternate screen, so the user \
         never sees it: restore at {restored}, message at {reported}"
    );
}
