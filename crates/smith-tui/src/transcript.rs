//! Memoised, virtualised transcript rendering.
//!
//! The transcript used to be rebuilt from scratch on every frame: every
//! assistant message re-parsed from markdown, every line re-wrapped cell by
//! cell, every tool card re-rendered, then `Paragraph::line_count` re-measured
//! the whole document just to work out `max_scroll` — and finally
//! `Paragraph::scroll` threw away everything above the offset. The cost was
//! O(whole session) per keystroke and per streaming delta.
//!
//! Three things fix that, and they only work together:
//!
//! 1. **Memo.** Each `ChatLine` renders to a self-contained run of rows
//!    (blank-line separators included), cached against the line's
//!    [`LineStamp`](crate::app::ChatLine) and the [`LayoutKey`].
//! 2. **Height index.** `offsets` is a prefix sum over those runs, so the
//!    document height is a single lookup and finding the row at a scroll
//!    offset is a binary search — no re-measure.
//! 3. **Virtualisation.** [`TranscriptCache::window`] hands back only the rows
//!    the viewport can show, borrowed from the cache rather than cloned.
//!
//! Rendering stays a pure function of `(lines, width, verbose, theme)`, which
//! is what keeps `TestBackend` snapshots reproducible.

use ratatui::text::{Line, Span};

use crate::app::{ChatLine, LineStamp};
use crate::theme::Theme;

/// Everything a rendered transcript depends on besides the `ChatLine`s
/// themselves. Any change here invalidates the whole memo, because width
/// drives wrapping, `verbose` drives tool-card bodies, and the theme drives
/// every span style.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutKey {
    pub width: u16,
    pub verbose: bool,
    pub theme: Theme,
}

struct Entry {
    stamp: LineStamp,
    /// A running tool card draws a spinner frame and a live elapsed counter,
    /// so its rendered form is a function of the clock and not of the
    /// `ChatLine`. Such an entry is rebuilt every frame regardless of stamp —
    /// there are at most a handful of them and they never touch markdown.
    animating: bool,
    rows: Vec<Line<'static>>,
}

/// Per-`ChatLine` memo of rendered rows plus the height index over them.
#[derive(Default)]
pub struct TranscriptCache {
    key: Option<LayoutKey>,
    entries: Vec<Entry>,
    /// `offsets[i]` is the first row of `entries[i]`; there are
    /// `entries.len() + 1` elements, so the last one is the total height of
    /// the finished transcript. Strictly increasing — every block contributes
    /// at least its trailing blank row.
    offsets: Vec<usize>,
    /// The streaming reply, keyed on its exact text. This is the hot path:
    /// it changes on every delta, so it is cached apart from the finished
    /// lines and re-rendered only when the string actually differs.
    in_flight: Option<(String, Vec<Line<'static>>)>,
    /// Instrumentation for the tests below: entries rebuilt by the last
    /// `sync`, and whether that sync re-rendered the streaming reply.
    misses: usize,
    in_flight_miss: bool,
}

impl TranscriptCache {
    /// Brings the memo in line with the current transcript. Cheap when
    /// nothing changed: one stamp comparison per `ChatLine` and no
    /// allocation.
    pub fn sync(
        &mut self,
        lines: &[ChatLine],
        in_flight: Option<&str>,
        key: &LayoutKey,
        spinner_frame: usize,
    ) {
        self.misses = 0;
        self.in_flight_miss = false;

        if self.key.as_ref() != Some(key) {
            self.entries.clear();
            self.in_flight = None;
            self.key = Some(key.clone());
        }

        // A shorter transcript (e.g. `/clear`) must not leave rows behind.
        self.entries.truncate(lines.len());

        let mut heights_changed = self.offsets.len() != lines.len() + 1;
        for (i, line) in lines.iter().enumerate() {
            if self
                .entries
                .get(i)
                .is_some_and(|e| e.stamp == line.stamp() && !e.animating)
            {
                continue;
            }
            let entry = Entry {
                stamp: line.stamp(),
                animating: line.is_animating(),
                rows: crate::ui::render_chat_line(
                    line,
                    &key.theme,
                    key.width,
                    key.verbose,
                    spinner_frame,
                ),
            };
            self.misses += 1;
            match self.entries.get_mut(i) {
                Some(slot) => {
                    heights_changed |= slot.rows.len() != entry.rows.len();
                    *slot = entry;
                }
                None => {
                    heights_changed = true;
                    self.entries.push(entry);
                }
            }
        }

        if heights_changed {
            self.offsets.clear();
            self.offsets.reserve(self.entries.len() + 1);
            let mut row = 0usize;
            self.offsets.push(row);
            for entry in &self.entries {
                row += entry.rows.len();
                self.offsets.push(row);
            }
        }

        match in_flight {
            // Markdown is context-sensitive — a later line can change how an
            // earlier one parses (a `|---|` turns the row above it into a
            // table header; a fence swallows everything after it) — so the
            // growing string is re-parsed whole. What we *can* skip is every
            // frame where the text did not actually move: spinner ticks,
            // keystrokes, tool activity.
            Some(text) => {
                let hit = self
                    .in_flight
                    .as_ref()
                    .is_some_and(|(cached, _)| cached == text);
                if !hit {
                    self.in_flight_miss = true;
                    self.in_flight = Some((
                        text.to_string(),
                        crate::ui::render_in_flight(text, &key.theme, key.width),
                    ));
                }
            }
            None => self.in_flight = None,
        }
    }

    /// Total rows in the document, including the streaming reply. O(1).
    pub fn total_height(&self) -> usize {
        self.finished_height() + self.in_flight.as_ref().map_or(0, |(_, rows)| rows.len())
    }

    fn finished_height(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0)
    }

    /// Rows `[from, from + count)` of the document, borrowed from the memo.
    /// This is the virtualisation: the caller renders a viewport, never the
    /// whole transcript.
    pub fn window(&self, from: usize, count: usize) -> Vec<Line<'_>> {
        let mut out: Vec<Line<'_>> = Vec::with_capacity(count);
        if count == 0 {
            return out;
        }

        // Last entry whose first row is at or before `from`. `offsets` is
        // strictly increasing, so this is unambiguous.
        let mut idx = self
            .offsets
            .partition_point(|&o| o <= from)
            .saturating_sub(1);
        let mut skip = from.saturating_sub(self.offsets.get(idx).copied().unwrap_or(0));
        while idx < self.entries.len() && out.len() < count {
            for row in self.entries[idx].rows.iter().skip(skip) {
                if out.len() == count {
                    break;
                }
                out.push(borrow_line(row));
            }
            skip = 0;
            idx += 1;
        }

        if out.len() < count {
            if let Some((_, rows)) = &self.in_flight {
                let next = from + out.len();
                let start = next.saturating_sub(self.finished_height());
                for row in rows.iter().skip(start) {
                    if out.len() == count {
                        break;
                    }
                    out.push(borrow_line(row));
                }
            }
        }

        out
    }

    /// `(first row, row count)` of the `i`th `ChatLine` in the document, or
    /// `None` if the memo doesn't cover it yet.
    ///
    /// This is the height index answering a question it was already carrying:
    /// scrolling a selected card into view needs its extent, and re-measuring
    /// the document to find it would undo the whole point of the memo.
    pub fn entry_rows(&self, index: usize) -> Option<(usize, usize)> {
        let start = *self.offsets.get(index)?;
        let end = *self.offsets.get(index + 1)?;
        Some((start, end - start))
    }

    /// Which entry occupies document row `row`, if any.
    ///
    /// `offsets` is a prefix sum, so this is a binary search rather than a
    /// walk — the same index that makes the height lookup O(1) makes a click
    /// O(log n) instead of O(session).
    pub fn entry_at_row(&self, row: usize) -> Option<usize> {
        if self.offsets.len() < 2 || row >= *self.offsets.last()? {
            return None;
        }
        // `partition_point` gives the first offset strictly greater than
        // `row`; the entry that contains the row is the one before it.
        Some(self.offsets.partition_point(|&o| o <= row) - 1)
    }

    /// Entries rebuilt by the most recent `sync` — 0 means a full cache hit.
    pub fn misses(&self) -> usize {
        self.misses
    }

    /// Whether the most recent `sync` re-rendered the streaming reply.
    pub fn in_flight_miss(&self) -> bool {
        self.in_flight_miss
    }
}

/// Re-points a cached row's spans at the cache's own strings, so handing a
/// viewport to ratatui costs one `Vec` per row instead of a `String` per span.
fn borrow_line<'a>(line: &'a Line<'static>) -> Line<'a> {
    let mut out = Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.as_ref(), s.style))
            .collect::<Vec<_>>(),
    );
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ActivityStatus, ChatLine, ChatRole};

    fn key(width: u16, verbose: bool) -> LayoutKey {
        LayoutKey {
            width,
            verbose,
            theme: Theme::ansi(),
        }
    }

    fn transcript(n: usize) -> Vec<ChatLine> {
        (0..n)
            .map(|i| {
                let role = if i % 2 == 0 {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                };
                ChatLine::new(role, format!("mensagem **{i}** com algum texto de corpo"))
            })
            .collect()
    }

    #[test]
    fn a_second_sync_with_nothing_changed_rebuilds_nothing() {
        let lines = transcript(40);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        assert_eq!(cache.misses(), 40);
        cache.sync(&lines, None, &key(60, false), 0);
        assert_eq!(cache.misses(), 0, "a settled transcript must be a full hit");
    }

    #[test]
    fn markdown_is_parsed_once_per_message_not_once_per_frame() {
        let lines = transcript(200);
        let mut cache = TranscriptCache::default();

        crate::markdown::reset_render_calls();
        cache.sync(&lines, None, &key(80, false), 0);
        let first = crate::markdown::render_calls();
        assert_eq!(
            first, 100,
            "one parse per assistant message on a cold cache"
        );

        crate::markdown::reset_render_calls();
        for _ in 0..10 {
            cache.sync(&lines, None, &key(80, false), 0);
        }
        assert_eq!(
            crate::markdown::render_calls(),
            0,
            "ten more frames must not re-parse a single message"
        );
    }

    fn test_app() -> crate::app::App {
        crate::app::App::new(crate::app::TuiConfig {
            banner: String::new(),
            provider_label: "ollama".into(),
            model_label: "qwen2.5".into(),
            cwd_display: "~/smith".into(),
            git_branch: None,
            idle_hint: crate::app::IdleHint::Tip(String::new()),
            initial_lines: Vec::new(),
            permission_policy: smith_core::PermissionPolicy::default(),
            theme: crate::Theme::ansi(),
            goal: None,
            tasks: Vec::new(),
            commands: crate::slash::SlashRegistry::builtin(),
            keys: Default::default(),
            history: Vec::new(),
            logs: crate::logbuf::LogBuffer::default(),
        })
    }

    fn rendered(cache: &TranscriptCache) -> String {
        cache
            .window(0, cache.total_height())
            .iter()
            .map(Line::to_string)
            .collect()
    }

    #[test]
    fn finishing_a_tool_call_invalidates_its_card() {
        // The stale-card bug: a tool line is mutated in place, so content
        // equality alone would keep serving the "running" spinner forever.
        let mut app = test_app();
        app.on_agent_event(smith_core::AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });

        let mut cache = TranscriptCache::default();
        cache.sync(&app.lines, None, &key(60, false), 0);
        assert!(
            rendered(&cache).contains('⠋'),
            "card should be spinning: {}",
            rendered(&cache)
        );

        app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "ok".into(),
            is_error: false,
        });
        cache.sync(&app.lines, None, &key(60, false), 0);
        let done = rendered(&cache);
        assert!(
            done.contains('✓') && !done.contains('⠋'),
            "the finished card was served from a stale memo: {done}"
        );
    }

    #[test]
    fn mutating_a_settled_card_invalidates_it_too() {
        // `finishing_a_tool_call_invalidates_its_card` would still pass on the
        // strength of the animating-card bypass alone. This one can't: the
        // card is already `Done` — and so memoisable — when it is mutated, so
        // only the stamp bump inside the mutator can save it.
        let mut app = test_app();
        app.verbose_tools = true;
        app.on_agent_event(smith_core::AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        for output in ["primeiro resultado", "segundo resultado"] {
            app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
                id: "call_1".into(),
                output: output.into(),
                is_error: false,
            });
        }
        let mut cache = TranscriptCache::default();
        cache.sync(&app.lines, None, &key(60, true), 0);
        assert!(rendered(&cache).contains("segundo"));

        // Settled card, cached, then mutated again.
        app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "terceiro resultado".into(),
            is_error: false,
        });
        cache.sync(&app.lines, None, &key(60, true), 0);
        let text = rendered(&cache);
        assert_eq!(cache.misses(), 1, "the mutated card was not invalidated");
        assert!(text.contains("terceiro"), "stale card content: {text}");
    }

    #[test]
    fn a_failed_turn_only_invalidates_the_cards_it_actually_touched() {
        let mut app = test_app();
        app.lines
            .push(ChatLine::new(ChatRole::Assistant, "uma resposta qualquer"));
        app.on_agent_event(smith_core::AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "ok".into(),
            is_error: false,
        });
        let mut cache = TranscriptCache::default();
        cache.sync(&app.lines, None, &key(60, false), 0);

        app.on_agent_event(smith_core::AgentEvent::Error("boom".into()));
        cache.sync(&app.lines, None, &key(60, false), 0);
        // Only the new "error:" system row — the settled assistant reply and
        // the already-done card must not be re-rendered.
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn a_running_card_is_never_served_from_the_memo() {
        // Its header carries a live elapsed counter, so a hit would freeze it.
        let lines = vec![ChatLine::test_tool(
            "read_file",
            ActivityStatus::Running,
            serde_json::json!({"path": "src/main.rs"}),
            None,
        )];
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        cache.sync(&lines, None, &key(60, false), 1);
        assert_eq!(cache.misses(), 1);
        let text: String = cache.window(0, 20).iter().map(Line::to_string).collect();
        assert!(text.contains('⠙'), "spinner frame did not advance: {text}");
    }

    #[test]
    fn changing_the_width_invalidates_everything() {
        let lines = transcript(10);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        cache.sync(&lines, None, &key(50, false), 0);
        assert_eq!(cache.misses(), 10);
    }

    #[test]
    fn toggling_verbose_invalidates_everything() {
        let lines = transcript(10);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        cache.sync(&lines, None, &key(60, true), 0);
        assert_eq!(cache.misses(), 10);
    }

    #[test]
    fn changing_the_theme_invalidates_everything() {
        let lines = transcript(10);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        let mut truecolor = key(60, false);
        truecolor.theme = Theme::truecolor();
        cache.sync(&lines, None, &truecolor, 0);
        assert_eq!(cache.misses(), 10);
    }

    #[test]
    fn streaming_text_is_re_rendered_only_when_it_actually_grows() {
        let lines = transcript(50);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, Some("resposta parcial"), &key(60, false), 0);
        assert!(cache.in_flight_miss());

        crate::markdown::reset_render_calls();
        cache.sync(&lines, Some("resposta parcial"), &key(60, false), 0);
        assert!(!cache.in_flight_miss());
        assert_eq!(cache.misses(), 0);
        assert_eq!(
            crate::markdown::render_calls(),
            0,
            "an unchanged stream must not re-parse anything"
        );

        cache.sync(&lines, Some("resposta parcial e mais"), &key(60, false), 0);
        assert!(cache.in_flight_miss());
        assert_eq!(
            crate::markdown::render_calls(),
            1,
            "a delta costs exactly one parse — of the reply, not the session"
        );
    }

    #[test]
    fn shrinking_the_transcript_drops_the_rows_it_owned() {
        let lines = transcript(10);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, None, &key(60, false), 0);
        let tall = cache.total_height();
        cache.sync(&lines[..3], None, &key(60, false), 0);
        assert!(cache.total_height() < tall);
        assert_eq!(cache.misses(), 0, "the surviving prefix is still valid");
    }

    #[test]
    fn the_height_index_matches_the_rows_actually_produced() {
        let mut lines = transcript(12);
        lines.push(ChatLine::test_tool(
            "run_bash",
            ActivityStatus::Done,
            serde_json::json!({"command": "cargo test"}),
            Some("ok"),
        ));
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, Some("streaming…"), &key(60, false), 0);
        let total = cache.total_height();
        assert_eq!(cache.window(0, total + 50).len(), total);
        assert_eq!(cache.window(total, 10).len(), 0);
    }

    #[test]
    fn a_window_is_the_same_slice_the_whole_document_would_give() {
        let lines = transcript(30);
        let mut cache = TranscriptCache::default();
        cache.sync(&lines, Some("cauda em streaming"), &key(60, false), 0);
        let all: Vec<String> = cache
            .window(0, cache.total_height())
            .iter()
            .map(Line::to_string)
            .collect();
        for from in [0usize, 1, 7, all.len().saturating_sub(3)] {
            let slice: Vec<String> = cache.window(from, 5).iter().map(Line::to_string).collect();
            assert_eq!(slice, all[from..(from + 5).min(all.len())].to_vec());
        }
    }

    #[test]
    fn no_row_ever_exceeds_the_pane_width() {
        let mut lines = vec![ChatLine::new(ChatRole::User, "palavra ".repeat(60))];
        lines.push(ChatLine::new(
            ChatRole::Assistant,
            format!("| a | b |\n| --- | --- |\n| {} | y |", "x".repeat(120)),
        ));
        lines.push(ChatLine::new(ChatRole::System, "s".repeat(200)));
        lines.push(ChatLine::test_tool(
            "run_bash",
            ActivityStatus::Error,
            serde_json::json!({"command": "cargo test ".repeat(20)}),
            Some(&"boom ".repeat(60)),
        ));
        let mut cache = TranscriptCache::default();
        for width in [24u16, 40, 61, 100] {
            cache.sync(&lines, Some(&"z".repeat(300)), &key(width, true), 0);
            for row in cache.window(0, cache.total_height()) {
                assert!(
                    row.width() <= width as usize,
                    "width {width}: row was {} wide: {row}",
                    row.width()
                );
            }
        }
    }
}
