//! Stripping `<think>` blocks out of the text channel.

/// Reasoning markup a model may emit in the *text* channel when the provider
/// exposes no separate reasoning stream — the shape every open reasoning
/// fine-tune has converged on.
const REASONING_TAGS: [&str; 3] = ["think", "thinking", "reasoning"];

/// What [`ReasoningFilter::scan_tag`] made of the `<` it is sitting on.
enum TagScan {
    /// The buffer ends mid-candidate; hold it and wait for the next delta.
    Incomplete,
    /// Ordinary text that merely starts with `<`.
    NotATag,
    Tag {
        len: usize,
        open: bool,
    },
}

/// Removes `<think>`/`<thinking>`/`<reasoning>` blocks from a *streamed* text
/// channel, so reasoning never reaches the transcript, history, or the next
/// request's prompt.
///
/// It lives here, in the one funnel every provider's text deltas pass through,
/// rather than in each adapter. The leak is a property of the *model*, not of
/// the wire format: the same weights served over Ollama, an OpenAI-compatible
/// proxy, or anything else emit the same tags, so a per-adapter fix would have
/// to be written once per adapter and would still miss the next one. Providers
/// that do have a real reasoning channel are unaffected — they never put the
/// tags in the text channel in the first place.
///
/// Two rules keep it from eating legitimate text, and both fail *open* (a tag
/// survives) rather than closed (prose is deleted):
///
/// - Nothing inside a ``` fenced block is markup, and neither is a tag
///   immediately preceded by a backtick — which is how a document about
///   reasoning tags, including this codebase's own, writes them.
/// - A closing tag with no opener removes only the tag. The opener is usually
///   lost upstream (consumed as a role marker by a chat template), so the
///   surrounding text is the model's real output and deleting it back to the
///   start of the message would throw away the reply.
pub(super) struct ReasoningFilter {
    /// Text held back because it may be the start of a tag or fence marker
    /// that continues in the next delta.
    buf: String,
    /// Open blocks. Counted rather than a flag so a nested tag can't close the
    /// outer block early.
    depth: u32,
    in_fence: bool,
    at_line_start: bool,
    /// Last raw character consumed, for the backtick check.
    prev: Option<char>,
    /// Tags removed so far — reported so a caller can note that reasoning was
    /// dropped rather than silently losing the fact.
    pub(super) stripped: u32,
}

impl ReasoningFilter {
    pub(super) fn new() -> Self {
        Self {
            buf: String::new(),
            depth: 0,
            in_fence: false,
            at_line_start: true,
            prev: None,
            stripped: 0,
        }
    }

    /// Feeds one streamed delta; returns the part safe to emit now.
    pub(super) fn push(&mut self, delta: &str) -> String {
        self.buf.push_str(delta);
        self.drain(false)
    }

    /// Flushes at end of stream. Anything still inside an unclosed block is
    /// dropped: the model marked it as reasoning itself, and a truncated
    /// thought is the least useful thing that could reach the transcript. If
    /// that empties the message, `run_turn`'s empty-turn retry picks it up.
    pub(super) fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn emit(&mut self, s: &str, out: &mut String) {
        if self.depth == 0 {
            out.push_str(s);
        }
        self.advance(s);
    }

    /// Line/backtick bookkeeping, which tracks the *raw* stream — suppressed
    /// text still moves the cursor.
    fn advance(&mut self, s: &str) {
        for ch in s.chars() {
            self.at_line_start = ch == '\n' || (self.at_line_start && ch.is_whitespace());
            self.prev = Some(ch);
        }
    }

    fn drain(&mut self, eos: bool) -> String {
        let buf = std::mem::take(&mut self.buf);
        let mut out = String::new();
        let mut cursor = 0;

        while cursor < buf.len() {
            let rest = &buf[cursor..];
            let Some(off) = rest.find(['<', '`']) else {
                self.emit(rest, &mut out);
                cursor = buf.len();
                break;
            };
            if off > 0 {
                self.emit(&rest[..off], &mut out);
                cursor += off;
                continue;
            }

            if rest.starts_with('`') {
                let run = rest.chars().take_while(|c| *c == '`').count();
                if run == rest.len() && !eos {
                    // The run may continue into the next delta and only then
                    // reach the three that open a fence.
                    break;
                }
                if run >= 3 && self.at_line_start {
                    self.in_fence = !self.in_fence;
                }
                self.emit(&rest[..run], &mut out);
                cursor += run;
                continue;
            }

            match self.scan_tag(rest, eos) {
                TagScan::Incomplete => break,
                TagScan::NotATag => {
                    self.emit("<", &mut out);
                    cursor += 1;
                }
                TagScan::Tag { len, open } => {
                    if open {
                        self.depth += 1;
                    } else {
                        self.depth = self.depth.saturating_sub(1);
                    }
                    self.stripped += 1;
                    // Consumed but never emitted — `advance` still runs so the
                    // line/backtick state stays aligned with the raw stream.
                    self.advance(&rest[..len]);
                    cursor += len;
                }
            }
        }

        self.buf = buf[cursor..].to_string();
        out
    }

    /// Classifies the `<` at the head of `rest`.
    fn scan_tag(&self, rest: &str, eos: bool) -> TagScan {
        if self.in_fence || self.prev == Some('`') {
            return TagScan::NotATag;
        }
        let after = &rest[1..];
        let (open, body) = match after.strip_prefix('/') {
            Some(body) => (false, body),
            None => (true, after),
        };
        let name_len = body
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(body.len());
        let name = body[..name_len].to_ascii_lowercase();
        // Bounded by the longest tag name, so a stray `<` can never hold the
        // stream open indefinitely.
        if !REASONING_TAGS.iter().any(|t| t.starts_with(&name)) {
            return TagScan::NotATag;
        }
        if name_len == body.len() {
            // Ran out of input mid-name.
            return if eos {
                TagScan::NotATag
            } else {
                TagScan::Incomplete
            };
        }
        if !body[name_len..].starts_with('>') || !REASONING_TAGS.contains(&name.as_str()) {
            return TagScan::NotATag;
        }
        let len = 1 + usize::from(!open) + name_len + 1;
        TagScan::Tag { len, open }
    }
}
