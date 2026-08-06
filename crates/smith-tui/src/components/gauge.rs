//! Context-window occupancy gauge — see `docs/design-system.md` §2.12.
//!
//! Fed by `AgentEvent::ContextUsage { used, window, estimated }`. Everything
//! here is a pure function of those three fields plus the theme, so the
//! thresholds and the label are unit-testable without a terminal.
//!
//! The gauge never animates. It changes only when a `ContextUsage` event
//! lands, which is what keeps `App::is_animating()` false at rest — a vital
//! that ticks would cost eight redraws a second forever.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::LineGauge;

use crate::theme::Theme;

/// Occupancy at which the gauge stops being reassuring.
const WARN_AT: f64 = 0.60;
/// Occupancy at which a compaction is imminent.
const DANGER_AT: f64 = 0.85;

/// Filled fraction of the window, clamped to `0..=1` — `LineGauge::ratio`
/// panics outside it, and a provider that reports `used > window` right before
/// a compaction is a real thing rather than a bug to propagate.
pub fn ratio(used: u32, window: u32) -> f64 {
    if window == 0 {
        return 0.0;
    }
    (f64::from(used) / f64::from(window)).clamp(0.0, 1.0)
}

/// Progressive colour for the filled part. Three steps rather than a gradient:
/// the question the user is asking is "do I need to do something about this
/// yet", and that has three answers.
pub fn fill_style(ratio: f64, theme: &Theme) -> Style {
    if ratio >= DANGER_AT {
        theme.danger()
    } else if ratio >= WARN_AT {
        theme.warning()
    } else {
        theme.success()
    }
}

/// `62% 79k/128k`, or `~62% 79k/128k` when part of the count is estimated.
///
/// The real numbers are in the label because a bare ratio throws away exactly
/// what the user wants to read, and cannot be recovered from. The leading `~`
/// is not decoration: `used` is the provider's own prompt count and is exact
/// up to the last response, but once tool results have been appended part of
/// it is a `chars/4` estimate of the unsent delta. Rendering an estimate in
/// the same shape as a measurement is a lie about which one this is.
pub fn label(used: u32, window: u32, estimated: bool) -> String {
    let percent = (ratio(used, window) * 100.0).round() as u32;
    let tilde = if estimated { "~" } else { "" };
    format!(
        "{tilde}{percent}% {}/{}",
        compact(used),
        compact(window.max(used))
    )
}

/// Spelled out under the gauge whenever `estimated` is set, because a tilde
/// alone does not tell anyone *what* is being estimated.
pub const ESTIMATE_LEGEND: &str = "~ est. since last reply";

/// `512`, `8.1k`, `128k` — short enough that the label leaves the bar room to
/// be a bar, precise enough to still be the number.
fn compact(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", f64::from(n) / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

/// The widget itself. Symbols come from the theme's glyph set so the bar is
/// still a bar on a terminal with no box-drawing characters.
pub fn context_gauge(used: u32, window: u32, estimated: bool, theme: &Theme) -> LineGauge<'static> {
    let ratio = ratio(used, window);
    let (filled, unfilled) = theme.gauge_symbols();
    let text = label(used, window, estimated);
    LineGauge::default()
        .ratio(ratio)
        .label(Line::from(Span::styled(text, theme.secondary())))
        .filled_symbol(filled)
        .unfilled_symbol(unfilled)
        .filled_style(fill_style(ratio, theme))
        .unfilled_style(theme.disabled())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ratio_is_clamped_at_both_ends() {
        assert_eq!(ratio(0, 1000), 0.0);
        assert_eq!(ratio(500, 1000), 0.5);
        // A provider can report a prompt bigger than the window right before
        // it compacts; `LineGauge::ratio` panics on anything above 1.0.
        assert_eq!(ratio(4000, 1000), 1.0);
        assert_eq!(ratio(100, 0), 0.0, "an unknown window must not divide");
    }

    #[test]
    fn the_colour_steps_at_sixty_and_eighty_five_percent() {
        let theme = Theme::ansi();
        assert_eq!(fill_style(0.0, &theme), theme.success());
        assert_eq!(fill_style(0.599, &theme), theme.success());
        assert_eq!(fill_style(0.60, &theme), theme.warning());
        assert_eq!(fill_style(0.849, &theme), theme.warning());
        assert_eq!(fill_style(0.85, &theme), theme.danger());
        assert_eq!(fill_style(1.0, &theme), theme.danger());
    }

    #[test]
    fn the_label_names_both_real_numbers() {
        assert_eq!(label(79_232, 128_000, false), "62% 79k/128k");
        assert_eq!(label(8_123, 128_000, false), "6% 8.1k/128k");
        assert_eq!(label(512, 8_192, false), "6% 512/8.2k");
    }

    #[test]
    fn an_estimate_is_never_rendered_as_a_measurement() {
        let measured = label(79_232, 128_000, false);
        let estimated = label(79_232, 128_000, true);
        assert_ne!(measured, estimated);
        assert!(estimated.starts_with('~'), "{estimated}");
        assert!(!measured.contains('~'), "{measured}");
    }

    #[test]
    fn the_label_never_claims_more_than_a_full_window() {
        // `used > window` is real; showing "104% 133k/128k" is not useful, and
        // showing "100% 133k/128k" is self-contradictory.
        assert_eq!(label(133_000, 128_000, false), "100% 133k/133k");
    }

    #[test]
    fn the_gauge_degrades_to_ascii_with_the_theme() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        for (theme, forbidden) in [(Theme::ansi(), '#'), (Theme::ansi().ascii_glyphs(), '━')] {
            let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
            terminal
                .draw(|f| {
                    f.render_widget(
                        context_gauge(6_000, 10_000, false, &theme),
                        Rect::new(0, 0, 40, 1),
                    );
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("60% 6.0k/10k"), "{text}");
            assert!(!text.contains(forbidden), "wrong glyph set: {text}");
            if theme.unicode {
                assert!(text.contains('━'), "{text}");
            } else {
                assert!(text.is_ascii(), "{text}");
            }
        }
    }

    #[test]
    fn the_filled_run_grows_with_the_ratio() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;

        let theme = Theme::ansi();
        let filled_cells = |used: u32| {
            let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
            terminal
                .draw(|f| {
                    f.render_widget(
                        context_gauge(used, 10_000, false, &theme),
                        Rect::new(0, 0, 40, 1),
                    );
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .filter(|c| c.symbol() == "━")
                .count()
        };
        assert!(filled_cells(2_000) < filled_cells(9_000));
    }
}
