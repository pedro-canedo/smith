use std::io::{self, Stdout};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init() -> io::Result<Tui> {
    enable_raw_mode()?;
    // Bracketed paste turns a pasted block into one `Event::Paste` instead of
    // a stream of keys — without it, pasting anything multi-line submits the
    // prompt at the first newline.
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    // Lets terminals that support it distinguish Shift+Enter from Enter.
    // Everywhere else the two are the same byte and Alt+Enter is the fallback.
    if keyboard_enhancement_available() {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    install_panic_hook();
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

pub fn restore() -> io::Result<()> {
    if keyboard_enhancement_available() {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let raw = disable_raw_mode();
    let screen = execute!(io::stdout(), Show, DisableBracketedPaste, LeaveAlternateScreen);
    raw.and(screen)?;
    Ok(())
}

/// Restores the terminal on every non-panic early return from the TUI loop.
/// The panic hook covers unwinding; this guard covers ordinary `Err` paths.
#[must_use]
pub struct RestoreGuard;

impl RestoreGuard {
    pub fn armed() -> Self {
        Self
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = restore();
    }
}

fn keyboard_enhancement_available() -> bool {
    supports_keyboard_enhancement().unwrap_or(false)
}

/// Ensures the terminal is left in a sane state even if we panic mid-render,
/// otherwise a crash leaves the user's shell stuck in raw/alternate-screen mode.
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore();
        original_hook(panic_info);
    }));
}
