//! One tool call as a card: header, progress, diff, output, errors.

use ratatui::text::{Line, Span};

use crate::app::{format_thought, ActivityStatus, ChatLine};
use crate::components::{diff, panel};
use crate::theme::Theme;

/// Error cards always surface the tail of the failure output, even in
/// compact mode — the reason a call broke should never be one keystroke away.
pub(super) const ERROR_TAIL_LINES: usize = 3;

/// Cap for tool output in verbose (expanded) mode.
pub(super) const VERBOSE_OUTPUT_CAP: usize = 12;

/// A tool call rendered as a raised card: one header row (icon, tool name,
/// target, duration) plus a context-dependent body — live command inset while
/// running, error tail on failure, full input/output/diff when verbose.
/// The throbber frame for one card.
///
/// A card with a `started_at` derives its phase from *its own* clock, so two
/// tools that began at different moments are visibly out of step — the
/// difference between the screen saying "something is happening" and "these
/// three things are happening". Cards with no start time (a restored
/// transcript, tests) fall back to the global counter.
///
/// This costs the render memo nothing: a `Running` card is already excluded
/// from it by `ChatLine::is_animating`, so it was being rebuilt every frame
/// regardless.
pub(super) fn spinner_frame_for(line: &ChatLine, global: usize, theme: &Theme) -> &'static str {
    let frames = theme.spinner_frames();
    let phase = match line.started_at() {
        Some(started) => (started.elapsed().as_millis() / crate::app::SPINNER_INTERVAL_MS) as usize,
        None => global,
    };
    frames[phase % frames.len()]
}

pub(super) fn tool_card(
    theme: &Theme,
    line: &ChatLine,
    area_width: u16,
    verbose: bool,
    spinner_frame: usize,
) -> Vec<Line<'static>> {
    let w = area_width as usize;
    // Selection is carried by the surface *and* by a marker glyph: on a
    // 16-colour terminal the elevation step can be invisible, and on a
    // monochrome one it certainly is.
    let selected = line.selected();
    let bg = if selected {
        theme.hover_bg()
    } else {
        theme.raised_bg()
    };
    let name = line.tool_name().unwrap_or_default().to_string();
    let target = tool_target(line);
    // Per-card expansion (`Enter`) on top of the global default.
    let verbose = verbose || line.expanded();
    // A card standing for a whole run of calls is a different card: its header
    // is a summary rather than a target, and its body is a step list that
    // collapses once the run settles.
    let grouped = !line.grouped().is_empty();
    let summary = line.group_summary();

    let (icon, icon_style) = match line.tool_status() {
        Some(ActivityStatus::Running) | None => (
            spinner_frame_for(line, spinner_frame, theme).to_string(),
            theme.ember(),
        ),
        Some(ActivityStatus::Done) => (theme.icon_ok().to_string(), theme.success()),
        Some(ActivityStatus::Error) => (theme.icon_error().to_string(), theme.danger()),
    };

    // The header speaks in activity labels, not tool names: "Searching the
    // web…" while running, "Search completed" when done, "Search failed" on
    // error. The raw name (`web_search`) moves to the verbose body.
    let labels = if grouped {
        crate::app::group_labels(&name)
    } else {
        crate::app::tool_labels(&name)
    };
    let (header_label, running_header) = match line.tool_status() {
        Some(ActivityStatus::Running) | None => (labels.running, true),
        Some(ActivityStatus::Done) => (labels.done, false),
        Some(ActivityStatus::Error) => (labels.failed, false),
    };

    let mut left = Vec::new();
    if selected {
        left.push(Span::styled(
            format!("{} ", theme.marker_selected()),
            theme.info_bold(),
        ));
    }
    left.extend([
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(header_label, theme.bold()),
    ]);
    if grouped {
        // The counts stand in for the target: on a run that mixed six queries
        // with four URLs, the first call's query described none of the others.
        let separator = theme.separator();
        left.push(Span::styled(
            format!(
                "{separator}{} {}",
                summary.steps,
                plural(summary.steps, "step")
            ),
            theme.secondary(),
        ));
        if summary.failed > 0 {
            // Named on the header itself, collapsed or not — this is what
            // lets `settle_group` stop marking a whole burst failed over one
            // 404 without the failure going quiet.
            left.push(Span::styled(
                format!("{separator}{} failed", summary.failed),
                theme.danger(),
            ));
        }
    } else if !target.is_empty() {
        // Running reads as a sentence ("Reading src/main.rs"); a settled card
        // separates verdict from target ("Read · src/main.rs").
        let separator = if running_header {
            " "
        } else {
            theme.separator()
        };
        left.push(Span::styled(
            format!("{separator}{target}"),
            match name.as_str() {
                "run_bash" => theme.text(),
                _ => theme.amber(),
            },
        ));
    }
    let left_width: usize = left.iter().map(|s| s.width()).sum();

    let duration = match line.tool_status() {
        Some(ActivityStatus::Running) | None => line
            .started_at()
            .map(|t| format!(" {} ", format_thought(t.elapsed().as_secs_f32())))
            .unwrap_or_default(),
        _ => line
            .tool_secs()
            .map(|s| format!(" {} ", format_thought(s)))
            .unwrap_or_default(),
    };
    // Only a settled group hides anything, so only a settled group claims the
    // affordance. While it runs, its steps are on screen and there is nothing
    // for the glyph to promise.
    let chevron = if grouped && !running_header {
        format!("{} ", theme.disclosure(verbose))
    } else {
        String::new()
    };
    let pad = w
        .saturating_sub(left_width + duration.chars().count() + chevron.chars().count())
        .max(1);
    left.push(Span::styled(" ".repeat(pad), theme.disabled()));
    left.push(Span::styled(duration, theme.disabled()));
    left.push(Span::styled(chevron, theme.info_bold()));

    let mut out = vec![panel::fill_line(left, w, bg)];

    // Every call the card stands for, one row each — its own first, now that
    // the header is a summary and no longer carries the first call's target.
    //
    // Live while the run is: that is the whole point, one card whose steps
    // tick by in place instead of a fresh card per call. Once it settles the
    // list folds away and the header is the entire card, until Enter or a
    // second click brings it back.
    if grouped && (running_header || verbose) {
        let branch = if theme.unicode {
            "\u{2514}\u{2500} "
        } else {
            "|- "
        };
        let steps = std::iter::once((tool_target(line), line.own_status())).chain(
            line.grouped()
                .iter()
                .map(|call| (call.label.clone(), Some(call.status))),
        );
        for (label, status) in steps {
            let (glyph, style) = match status {
                None | Some(ActivityStatus::Running) => (
                    spinner_frame_for(line, spinner_frame, theme).to_string(),
                    theme.ember(),
                ),
                Some(ActivityStatus::Done) => (theme.icon_ok().to_string(), theme.success()),
                Some(ActivityStatus::Error) => (theme.icon_error().to_string(), theme.danger()),
            };
            let text = truncate_chars(&label, w.saturating_sub(branch.chars().count() + 4));
            out.push(panel::fill_line(
                vec![
                    Span::styled(format!("  {branch}"), theme.disabled()),
                    Span::styled(format!("{glyph} "), style),
                    Span::styled(text, theme.secondary()),
                ],
                w,
                bg,
            ));
        }
    }

    let finished_error = matches!(line.tool_status(), Some(ActivityStatus::Error));
    let finished = matches!(
        line.tool_status(),
        Some(ActivityStatus::Done) | Some(ActivityStatus::Error)
    );

    // Running: show what the call is actually doing. A group says it in its
    // step list instead — repeating the newest target under it would say the
    // same thing twice.
    if !grouped && matches!(line.tool_status(), Some(ActivityStatus::Running) | None) {
        if name == "run_bash" {
            if let Some(cmd) = tool_field(line, "command") {
                let row = Line::from(vec![
                    Span::styled("$ ", theme.success()),
                    Span::styled(cmd.to_string(), theme.text()),
                ]);
                out.extend(panel::inset(&[row], w, theme.overlay_bg()));
            }
        } else if !target.is_empty() {
            let row = Line::from(Span::styled(target.clone(), theme.amber()));
            out.extend(panel::inset(&[row], w, theme.overlay_bg()));
        }
        // The newest `ToolProgress` line, so a long call visibly moves
        // instead of sitting on a spinner (`set_progress` keeps only the
        // latest). Before this, the line was stored and never shown.
        if let Some(progress) = line.tool_output().and_then(|o| o.lines().next_back()) {
            let row = Line::from(vec![
                Span::styled(format!("{} ", theme.ellipsis()), theme.disabled()),
                Span::styled(
                    truncate_chars(progress, w.saturating_sub(6)),
                    theme.secondary(),
                ),
            ]);
            out.extend(panel::inset(&[row], w, theme.overlay_bg()));
        }
    }

    // Errors always surface the tail of the failure output — except on a
    // collapsed group, whose contract is that the header is the whole card.
    // `tool_output` there is only the *own* call's anyway, so the tail would
    // describe one step out of ten.
    if finished_error && (!grouped || verbose) {
        for l in error_tail(line, ERROR_TAIL_LINES) {
            let mut spans = vec![Span::styled(
                theme.assistant_gutter().0.to_string(),
                theme.danger(),
            )];
            spans.push(Span::styled(
                truncate_chars(&l, w.saturating_sub(4)),
                theme.secondary(),
            ));
            out.push(panel::fill_line(spans, w, bg));
        }
    }

    // Verbose: full input + output (+ a real diff for edit_file).
    //
    // Never for a group: `tool_input` and `tool_output` there belong to its
    // own call alone, so this would answer "what did those ten steps do?" with
    // the raw name and output of the first one. Expanding a group already has
    // a meaning — the step list above — and that is the honest one.
    if verbose && finished && !grouped {
        // The raw tool name lives here now that the header speaks in
        // friendly labels — verbose is exactly the "what actually ran" view.
        out.extend(panel::inset(
            &[Line::from(Span::styled(name.clone(), theme.disabled()))],
            w,
            theme.overlay_bg(),
        ));
        match name.as_str() {
            "edit_file" => {
                let old = tool_field(line, "old_str").unwrap_or("");
                let new = tool_field(line, "new_str").unwrap_or("");
                if !old.is_empty() || !new.is_empty() {
                    out.extend(diff::render_diff(old, new, theme, w));
                }
            }
            "write_file" => {
                let mut rows = vec![];
                if let Some(path) = tool_field(line, "path") {
                    rows.push(Line::from(Span::styled(path.to_string(), theme.amber())));
                }
                if let Some(content) = tool_field(line, "content") {
                    for l in content.lines().take(ERROR_TAIL_LINES) {
                        rows.push(Line::from(Span::styled(l.to_string(), theme.secondary())));
                    }
                    if content.lines().count() > ERROR_TAIL_LINES {
                        rows.push(Line::from(Span::styled(
                            theme.ellipsis().to_string(),
                            theme.disabled(),
                        )));
                    }
                }
                if !rows.is_empty() {
                    out.extend(panel::inset(&rows, w, theme.overlay_bg()));
                }
            }
            _ => {
                let mut rows = vec![];
                if name == "run_bash" {
                    if let Some(cmd) = tool_field(line, "command") {
                        rows.push(Line::from(vec![
                            Span::styled("$ ", theme.success()),
                            Span::styled(cmd.to_string(), theme.text()),
                        ]));
                    }
                } else if !target.is_empty() {
                    rows.push(Line::from(Span::styled(target.clone(), theme.amber())));
                }
                if !rows.is_empty() {
                    out.extend(panel::inset(&rows, w, theme.overlay_bg()));
                }
            }
        }

        if let Some(output) = line.tool_output() {
            let total = output.lines().count();
            let cap = total.min(VERBOSE_OUTPUT_CAP);
            let style = if finished_error {
                theme.danger()
            } else {
                theme.secondary()
            };
            let rows: Vec<Line> = output
                .lines()
                .take(cap)
                .map(|l| Line::from(Span::styled(truncate_chars(l, w.saturating_sub(4)), style)))
                .collect();
            if !rows.is_empty() {
                out.extend(panel::inset(&rows, w, theme.overlay_bg()));
            }
            if total > cap {
                let row = Line::from(Span::styled(
                    format!("… +{} lines", total - cap),
                    theme.disabled(),
                ));
                out.extend(panel::inset(&[row], w, theme.overlay_bg()));
            }
        }
    }

    out
}

pub(super) fn tool_field<'a>(line: &'a ChatLine, key: &str) -> Option<&'a str> {
    line.tool_input()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// The human-readable target of a tool call (path / command / pattern /
/// query), for the card header.
pub(super) fn tool_target(line: &ChatLine) -> String {
    let name = line.tool_name().unwrap_or("");
    let target = match name {
        "read_file" | "write_file" | "edit_file" | "list_dir" => tool_field(line, "path"),
        "glob" => tool_field(line, "pattern"),
        "grep" => tool_field(line, "pattern"),
        "task" => tool_field(line, "description"),
        "multi_edit" => tool_field(line, "path"),
        "run_bash" => tool_field(line, "command"),
        "web_search" => tool_field(line, "query"),
        "web_fetch" => tool_field(line, "url"),
        _ => None,
    }
    .unwrap_or_default();
    truncate_chars(target, MAX_LABEL_CHARS_DISPLAY)
}

pub(super) const MAX_LABEL_CHARS_DISPLAY: usize = 64;

/// "1 step" / "2 steps" — English pluralisation, for the one word that needs
/// it. A count that reads "1 steps" is the sort of thing you stop seeing after
/// a week and no user ever does.
pub(super) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub(super) fn error_tail(line: &ChatLine, max: usize) -> Vec<String> {
    line.tool_output()
        .map(|o| {
            o.lines()
                .rev()
                .take(max)
                .map(str::to_string)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        })
        .unwrap_or_default()
}
