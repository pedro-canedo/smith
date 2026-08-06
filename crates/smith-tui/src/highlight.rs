//! A small, self-contained syntax highlighter for fenced code blocks.
//!
//! Deliberately hand-rolled. `syntect` — the obvious dependency — links `onig`
//! (a C library) by default, and its pure-Rust variant still carries ~1.5 MB of
//! serialised syntax dumps; both undermine the single-static-binary delivery
//! story this repo has chosen everywhere else (hand-rolled MCP client, no
//! `git2`, no `throbber-widgets-tui`). What a transcript actually contains is a
//! dozen languages' worth of *fragments*, not compilers' input, so an
//! approximate lexer that never lies about structure is enough.
//!
//! Three properties are load-bearing, and each has tests at the bottom:
//!
//! - **One pass, no backtracking.** [`tokenize`] walks the source once with a
//!   byte cursor that only ever moves forward, so a 5 000-line block costs what
//!   a 5-line one costs per character. `markdown::render` is memoised per
//!   message, but it still runs on every new message.
//! - **Char boundaries, never byte offsets.** Every advance goes through
//!   `Lexer::bump`, which steps by `char::len_utf8`. Accented text and emoji
//!   inside a code block are ordinary content, not a panic.
//! - **Multi-line constructs survive line splitting.** Tokens are produced over
//!   the whole block and cut into [`Line`]s afterwards ([`to_lines`]), so a
//!   triple-quoted string or a block comment stays coloured on its second line.
//!
//! An unknown or absent language returns `None`; the language is *never*
//! guessed from content. Colours come from `Theme` roles only — this module
//! contains no `Color::` literal, like everything outside `theme.rs`.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// What a run of characters is, in the only granularity a terminal palette can
/// express. Each maps to one existing Ember role in [`style_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Comment,
    Str,
    Number,
    Keyword,
    /// A type, class, tag, or key name — the "this names a shape" role.
    Type,
    /// A name at a call or definition site.
    Func,
    Punct,
    /// A plain identifier: a name that is none of the above.
    Ident,
    /// Whitespace and anything the lexer has no opinion about.
    Plain,
}

/// The role mapping. `theme.rs` is owned by another change; nothing here needs
/// a token it doesn't already have.
fn style_for(kind: Tok, theme: &Theme) -> Style {
    match kind {
        Tok::Comment => theme.disabled(),
        Tok::Str => theme.success(),
        Tok::Number => theme.plan(),
        Tok::Keyword => theme.ember(),
        Tok::Type => theme.warning(),
        Tok::Func => theme.info(),
        Tok::Punct => theme.secondary(),
        Tok::Ident | Tok::Plain => theme.text(),
    }
}

/// The languages a coding agent's transcript actually contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    /// JavaScript and TypeScript share one lexer and one keyword set.
    Js,
    Json,
    Toml,
    Yaml,
    Shell,
    Go,
    Sql,
    Html,
    Markdown,
}

impl Lang {
    /// Resolves a fence info string (` ```rust `, ` ```js {1,3} `, ` ```ts,ignore `).
    ///
    /// Only the first word counts, and only an exact alias matches: an
    /// unrecognised tag is `None`, never a guess.
    pub fn from_info(info: &str) -> Option<Self> {
        let word = info
            .trim()
            .split(|c: char| c.is_whitespace() || c == ',' || c == '{' || c == ':')
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| c == '.' || c == '`');
        if word.is_empty() {
            return None;
        }
        let lower = word.to_ascii_lowercase();
        Some(match lower.as_str() {
            "rust" | "rs" => Lang::Rust,
            "python" | "py" | "python3" => Lang::Python,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" | "node" | "typescript" | "ts" | "tsx" => {
                Lang::Js
            }
            "json" => Lang::Json,
            "toml" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "bash" | "sh" | "shell" | "zsh" | "ksh" => Lang::Shell,
            "go" | "golang" => Lang::Go,
            "sql" | "postgres" | "postgresql" | "psql" | "mysql" | "sqlite" => Lang::Sql,
            "html" | "htm" | "xhtml" => Lang::Html,
            "markdown" | "md" => Lang::Markdown,
            _ => return None,
        })
    }
}

/// Highlights `source` as `info` names it, one [`Line`] per source line.
///
/// Returns `None` — leave the text exactly as it was — when the info string
/// names no language this module knows.
pub fn highlight(source: &str, info: &str, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let lang = Lang::from_info(info)?;
    let tokens = tokenize(source, lang);
    Some(to_lines(source, &tokens, theme))
}

/// A half-open byte range and what it is. Ranges are produced in order and
/// never overlap.
type Token = (usize, usize, Tok);

fn tokenize(src: &str, lang: Lang) -> Vec<Token> {
    let mut lx = Lexer::new(src);
    match lang {
        Lang::Rust => c_like(&mut lx, &RUST),
        Lang::Js => c_like(&mut lx, &JS),
        Lang::Go => c_like(&mut lx, &GO),
        Lang::Python => python(&mut lx),
        Lang::Shell => shell(&mut lx),
        Lang::Json => json(&mut lx),
        Lang::Toml => toml(&mut lx),
        Lang::Yaml => yaml(&mut lx),
        Lang::Sql => sql(&mut lx),
        Lang::Html => html(&mut lx),
        Lang::Markdown => markdown(&mut lx),
    }
    lx.out
}

/// Cuts the token stream into one [`Line`] per source line.
///
/// Splitting *after* tokenising is what keeps a multi-line string or comment
/// coloured past its first newline. Any range the lexer failed to cover is
/// emitted as `Plain` rather than dropped, so the output always has exactly as
/// many lines as the input — a highlighter that silently loses a line would
/// misalign the whole transcript.
fn to_lines(src: &str, tokens: &[Token], theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut covered = 0usize;
    let push_text = |text: &str, kind: Tok, lines: &mut Vec<Line<'static>>, cur: &mut Vec<_>| {
        let style = style_for(kind, theme);
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::from(std::mem::take(cur)));
            }
            if !piece.is_empty() {
                cur.push(Span::styled(piece.to_string(), style));
            }
        }
    };
    for &(start, end, kind) in tokens {
        if start > covered {
            push_text(&src[covered..start], Tok::Plain, &mut lines, &mut cur);
        }
        push_text(&src[start..end], kind, &mut lines, &mut cur);
        covered = end;
    }
    if covered < src.len() {
        push_text(&src[covered..], Tok::Plain, &mut lines, &mut cur);
    }
    lines.push(Line::from(cur));
    lines
}

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// A forward-only cursor over the block, plus the tokens found so far.
///
/// `pos` is a byte index that is always on a char boundary, because it only
/// ever moves by `char::len_utf8`. Nothing here slices by a computed offset.
struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    out: Vec<Token>,
    /// Nesting depth of template-literal interpolation, so `` `${`${...}`}` ``
    /// cannot recurse without bound.
    depth: u8,
}

const MAX_INTERPOLATION_DEPTH: u8 = 8;

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            out: Vec::new(),
            depth: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn prev(&self) -> Option<char> {
        self.src[..self.pos].chars().next_back()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn bump_n(&mut self, n: usize) {
        for _ in 0..n {
            if self.bump().is_none() {
                break;
            }
        }
    }

    fn at(&self, s: &str) -> bool {
        self.rest().starts_with(s)
    }

    fn at_line_start(&self) -> bool {
        matches!(self.prev(), None | Some('\n'))
    }

    /// Records `start..pos` as `kind`, merging into the previous token when it
    /// is the same kind and abuts — fewer spans for the same pixels.
    fn push(&mut self, start: usize, kind: Tok) {
        if start >= self.pos {
            return;
        }
        if let Some(last) = self.out.last_mut() {
            if last.2 == kind && last.1 == start {
                last.1 = self.pos;
                return;
            }
        }
        self.out.push((start, self.pos, kind));
    }

    fn take_while(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.bump();
        }
    }

    /// Consumes to just before the next newline (or to the end).
    fn eat_line(&mut self) {
        self.take_while(|c| c != '\n');
    }

    /// The text of the current line, not consumed.
    fn line_ahead(&self) -> &'a str {
        let rest = self.rest();
        match rest.find('\n') {
            Some(i) => &rest[..i],
            None => rest,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared scanners. Each consumes; the caller decides the token's start and kind
// (so a prefix like Python's `rb` or Rust's `br#` joins the string it opens).
// ---------------------------------------------------------------------------

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Consumes a quoted run starting at the opening quote.
///
/// An unterminated quote stops at the newline (`multiline == false`) or at the
/// end of the block — never runs off, never loops.
fn scan_quoted(lx: &mut Lexer, quote: char, escapes: bool, multiline: bool) {
    lx.bump();
    while let Some(c) = lx.peek() {
        if c == '\n' && !multiline {
            return;
        }
        if escapes && c == '\\' {
            lx.bump();
            lx.bump();
            continue;
        }
        lx.bump();
        if c == quote {
            return;
        }
    }
}

/// Consumes up to and including `close`, from just after an opening delimiter.
fn scan_until(lx: &mut Lexer, close: &str, escapes: bool) {
    while !lx.eof() {
        if escapes && lx.at("\\") {
            lx.bump();
            lx.bump();
            continue;
        }
        if lx.at(close) {
            lx.bump_n(close.chars().count());
            return;
        }
        lx.bump();
    }
}

/// Consumes a block comment starting at its opening delimiter.
///
/// `nestable` is Rust's rule: `/* /* */ */` is one comment, and `/* /* */` is
/// unterminated — which means "to the end of the block", not "hang".
fn scan_block_comment(lx: &mut Lexer, open: &str, close: &str, nestable: bool) {
    lx.bump_n(open.chars().count());
    let mut depth = 1usize;
    while !lx.eof() {
        if nestable && lx.at(open) {
            lx.bump_n(open.chars().count());
            depth += 1;
            continue;
        }
        if lx.at(close) {
            lx.bump_n(close.chars().count());
            depth -= 1;
            if depth == 0 {
                return;
            }
            continue;
        }
        lx.bump();
    }
}

/// Consumes a numeric literal: bases, digit separators, a fraction, an
/// exponent, and any type suffix glued to the end (`1_000u32`, `0xFFu8`,
/// `1.5e-3`).
fn scan_number(lx: &mut Lexer) {
    if lx.at("0x") || lx.at("0X") || lx.at("0b") || lx.at("0B") || lx.at("0o") || lx.at("0O") {
        lx.bump_n(2);
    }
    lx.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
    if lx.peek() == Some('.') && lx.peek_nth(1).is_some_and(|c| c.is_ascii_digit()) {
        lx.bump();
        lx.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    if matches!(lx.peek(), Some('+' | '-')) && matches!(lx.prev(), Some('e' | 'E')) {
        lx.bump();
        lx.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
    }
}

/// Consumes an identifier and returns its range.
fn scan_word(lx: &mut Lexer) -> (usize, usize) {
    let start = lx.pos;
    lx.take_while(is_ident_part);
    (start, lx.pos)
}

/// Whether the next non-blank character on *this* line opens an argument list.
fn next_is_call(lx: &Lexer) -> bool {
    lx.rest()
        .chars()
        .find(|c| !matches!(c, ' ' | '\t'))
        .is_some_and(|c| c == '(')
}

/// Keyword, then built-in type, then call site, then the capitalised-name
/// convention, then a plain identifier.
fn classify(word: &str, cfg: &CLike, call: bool) -> Tok {
    classify_with(word, cfg.keywords, cfg.types, call)
}

fn classify_with(word: &str, keywords: &[&str], types: &[&str], call: bool) -> Tok {
    if keywords.binary_search(&word).is_ok() {
        return Tok::Keyword;
    }
    if types.binary_search(&word).is_ok() {
        return Tok::Type;
    }
    // A capitalised name is a type even at a call site: `Some(x)`, `Vec::new`
    // and `new Widget()` are constructors, and reading them as functions makes
    // the type/value distinction the colours exist for disappear.
    if word.chars().next().is_some_and(char::is_uppercase) {
        return Tok::Type;
    }
    if call {
        return Tok::Func;
    }
    Tok::Ident
}

// ---------------------------------------------------------------------------
// C-family: Rust, JavaScript/TypeScript, Go
// ---------------------------------------------------------------------------

struct CLike {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Rust nests block comments; C, Go and JS do not.
    nested_block_comments: bool,
    /// Rust's `r"..."` / `r#"..."#` / `b"..."` / `br#"..."#`.
    raw_hash_strings: bool,
    /// Rust's `'a` lifetime, which is *not* an unterminated char literal.
    lifetimes: bool,
    /// Go's backtick raw string.
    backtick_string: bool,
    /// JS template literal, with `${...}` lexed as code.
    template_string: bool,
    /// Rust string literals may contain a raw newline; Go's and JS's may not.
    multiline_strings: bool,
    /// Rust's `println!` — the `!` belongs to the name.
    macro_bang: bool,
}

const RUST: CLike = CLike {
    keywords: RUST_KEYWORDS,
    types: RUST_TYPES,
    nested_block_comments: true,
    raw_hash_strings: true,
    lifetimes: true,
    backtick_string: false,
    template_string: false,
    multiline_strings: true,
    macro_bang: true,
};

const JS: CLike = CLike {
    keywords: JS_KEYWORDS,
    types: JS_TYPES,
    nested_block_comments: false,
    raw_hash_strings: false,
    lifetimes: false,
    backtick_string: false,
    template_string: true,
    multiline_strings: false,
    macro_bang: false,
};

const GO: CLike = CLike {
    keywords: GO_KEYWORDS,
    types: GO_TYPES,
    nested_block_comments: false,
    raw_hash_strings: false,
    lifetimes: false,
    backtick_string: true,
    template_string: false,
    multiline_strings: false,
    macro_bang: false,
};

fn c_like(lx: &mut Lexer, cfg: &CLike) {
    while !lx.eof() {
        let before = lx.pos;
        c_like_step(lx, cfg);
        force_progress(lx, before);
    }
}

/// The termination guarantee: any step that consumed nothing is turned into one
/// character of `Plain`. No input can make the outer loop spin.
fn force_progress(lx: &mut Lexer, before: usize) {
    if lx.pos == before {
        lx.bump();
        lx.push(before, Tok::Plain);
    }
}

fn c_like_step(lx: &mut Lexer, cfg: &CLike) {
    let start = lx.pos;
    let Some(c) = lx.peek() else { return };
    match c {
        c if c.is_whitespace() => {
            lx.take_while(char::is_whitespace);
            lx.push(start, Tok::Plain);
        }
        '/' if lx.at("//") => {
            lx.eat_line();
            lx.push(start, Tok::Comment);
        }
        '/' if lx.at("/*") => {
            scan_block_comment(lx, "/*", "*/", cfg.nested_block_comments);
            lx.push(start, Tok::Comment);
        }
        '"' => {
            scan_quoted(lx, '"', true, cfg.multiline_strings);
            lx.push(start, Tok::Str);
        }
        '`' if cfg.backtick_string => {
            scan_quoted(lx, '`', false, true);
            lx.push(start, Tok::Str);
        }
        '`' if cfg.template_string => scan_template(lx, cfg),
        '\'' if cfg.lifetimes => rust_quote(lx),
        '\'' => {
            scan_quoted(lx, '\'', true, false);
            lx.push(start, Tok::Str);
        }
        'r' | 'b' if cfg.raw_hash_strings && rust_string_ahead(lx) => {
            rust_string(lx);
            lx.push(start, Tok::Str);
        }
        c if c.is_ascii_digit() => {
            scan_number(lx);
            lx.push(start, Tok::Number);
        }
        c if is_ident_start(c) => {
            let src = lx.src;
            let (s, e) = scan_word(lx);
            let word = &src[s..e];
            let kind = if cfg.macro_bang
                && lx.peek() == Some('!')
                && matches!(lx.peek_nth(1), Some('(' | '[' | '{'))
            {
                lx.bump();
                Tok::Func
            } else {
                classify(word, cfg, next_is_call(lx))
            };
            lx.push(s, kind);
        }
        _ => {
            lx.bump();
            lx.push(start, Tok::Punct);
        }
    }
}

/// Whether the cursor is on a Rust string prefix (`r"`, `r#"`, `b"`, `br#"`)
/// rather than on an identifier that merely begins with `r` or `b`.
fn rust_string_ahead(lx: &Lexer) -> bool {
    let rest = lx.rest();
    let after_prefix = if let Some(s) = rest.strip_prefix("br") {
        s
    } else if let Some(s) = rest.strip_prefix('r') {
        s
    } else if let Some(s) = rest.strip_prefix('b') {
        // `b"..."` is a byte string; `b'x'` and `buf` are not.
        return s.starts_with('"');
    } else {
        return false;
    };
    let hashes = after_prefix.len() - after_prefix.trim_start_matches('#').len();
    after_prefix[hashes..].starts_with('"')
}

/// Consumes a Rust string literal, raw or not, with any number of hashes.
fn rust_string(lx: &mut Lexer) {
    let mut raw = false;
    if lx.peek() == Some('b') {
        lx.bump();
    }
    if lx.peek() == Some('r') {
        raw = true;
        lx.bump();
    }
    let mut hashes = 0usize;
    while lx.peek() == Some('#') {
        hashes += 1;
        lx.bump();
    }
    if !raw {
        scan_quoted(lx, '"', true, true);
        return;
    }
    lx.bump(); // opening quote
    while !lx.eof() {
        if lx.peek() == Some('"') {
            lx.bump();
            let mut seen = 0usize;
            while seen < hashes && lx.peek() == Some('#') {
                lx.bump();
                seen += 1;
            }
            if seen == hashes {
                return;
            }
            // Fewer hashes than the opener: still inside the string, and every
            // character consumed stays consumed — no backtracking.
            continue;
        }
        lx.bump();
    }
}

/// Rust's `'`: a char literal, or a lifetime, which looks like an
/// unterminated one and must not swallow the rest of the line.
fn rust_quote(lx: &mut Lexer) {
    let start = lx.pos;
    match (lx.peek_nth(1), lx.peek_nth(2)) {
        (Some('\\'), _) => {
            scan_quoted(lx, '\'', true, false);
            lx.push(start, Tok::Str);
        }
        (Some(_), Some('\'')) => {
            lx.bump_n(3);
            lx.push(start, Tok::Str);
        }
        (Some(c), _) if is_ident_start(c) => {
            lx.bump();
            lx.take_while(is_ident_part);
            lx.push(start, Tok::Type);
        }
        _ => {
            lx.bump();
            lx.push(start, Tok::Punct);
        }
    }
}

/// A JS template literal. The `${ ... }` holes are lexed as ordinary code, so
/// `` `n = ${count + 1}` `` colours the expression rather than the quotes.
fn scan_template(lx: &mut Lexer, cfg: &CLike) {
    let mut chunk = lx.pos;
    lx.bump(); // opening backtick
    loop {
        match lx.peek() {
            None => break,
            Some('`') => {
                lx.bump();
                break;
            }
            Some('\\') => {
                lx.bump();
                lx.bump();
            }
            Some('$') if lx.peek_nth(1) == Some('{') && lx.depth < MAX_INTERPOLATION_DEPTH => {
                lx.push(chunk, Tok::Str);
                let open = lx.pos;
                lx.bump_n(2);
                lx.push(open, Tok::Punct);
                lx.depth += 1;
                let mut braces = 1usize;
                while braces > 0 && !lx.eof() {
                    let before = lx.pos;
                    match lx.peek() {
                        Some('{') => {
                            lx.bump();
                            braces += 1;
                            lx.push(before, Tok::Punct);
                        }
                        Some('}') => {
                            lx.bump();
                            braces -= 1;
                            lx.push(before, Tok::Punct);
                        }
                        _ => {
                            c_like_step(lx, cfg);
                            force_progress(lx, before);
                        }
                    }
                }
                lx.depth -= 1;
                chunk = lx.pos;
            }
            _ => {
                lx.bump();
            }
        }
    }
    lx.push(chunk, Tok::Str);
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn python(lx: &mut Lexer) {
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None => break,
            Some(c) if c.is_whitespace() => {
                lx.take_while(char::is_whitespace);
                lx.push(start, Tok::Plain);
            }
            Some('#') => {
                lx.eat_line();
                lx.push(start, Tok::Comment);
            }
            Some('"' | '\'') => {
                py_string(lx);
                lx.push(start, Tok::Str);
            }
            Some('@') if lx.at_line_start() || lx.prev() == Some(' ') => {
                lx.bump();
                lx.take_while(|c| is_ident_part(c) || c == '.');
                lx.push(start, Tok::Func);
            }
            Some(c) if c.is_ascii_digit() => {
                scan_number(lx);
                lx.push(start, Tok::Number);
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let word = &src[s..e];
                if is_py_string_prefix(word) && matches!(lx.peek(), Some('"' | '\'')) {
                    py_string(lx);
                    lx.push(s, Tok::Str);
                } else {
                    let kind = classify_with(word, PY_KEYWORDS, PY_TYPES, next_is_call(lx));
                    lx.push(s, kind);
                }
            }
            Some(_) => {
                lx.bump();
                lx.push(start, Tok::Punct);
            }
        }
        force_progress(lx, before);
    }
}

/// `r`, `b`, `f`, `u`, `rb`, `fr`, ... in any case.
fn is_py_string_prefix(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 2
        && word
            .chars()
            .all(|c| matches!(c.to_ascii_lowercase(), 'r' | 'b' | 'f' | 'u'))
}

/// Consumes a Python string, triple-quoted (and therefore multi-line) or not.
fn py_string(lx: &mut Lexer) {
    let quote = lx.peek().unwrap_or('"');
    let triple: String = std::iter::repeat_n(quote, 3).collect();
    if lx.at(&triple) {
        lx.bump_n(3);
        scan_until(lx, &triple, true);
        return;
    }
    scan_quoted(lx, quote, true, false);
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

fn shell(lx: &mut Lexer) {
    // The word position a command name occupies — after a newline, a pipe, a
    // semicolon or an `&&`. It is what makes `grep` in `ls | grep x` read as a
    // command and `x` read as an argument.
    let mut command_position = true;
    let mut heredoc: Option<(usize, usize, bool)> = None;
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None => break,
            Some('\n') => {
                lx.bump();
                lx.push(start, Tok::Plain);
                command_position = true;
                if let Some((ds, de, strip)) = heredoc.take() {
                    let body = lx.pos;
                    scan_heredoc(lx, ds, de, strip);
                    lx.push(body, Tok::Str);
                }
            }
            Some(c) if c.is_whitespace() => {
                lx.take_while(|c| c.is_whitespace() && c != '\n');
                lx.push(start, Tok::Plain);
            }
            // `#` opens a comment only at the start of a word: `$#` and
            // `file#1` are not comments.
            Some('#')
                if lx.at_line_start() || matches!(lx.prev(), Some(' ' | '\t' | ';' | '(')) =>
            {
                lx.eat_line();
                lx.push(start, Tok::Comment);
            }
            Some('\'') => {
                // Single quotes take no escapes at all, and may span lines.
                lx.bump();
                lx.take_while(|c| c != '\'');
                lx.bump();
                lx.push(start, Tok::Str);
                command_position = false;
            }
            Some('"') => {
                scan_quoted(lx, '"', true, true);
                lx.push(start, Tok::Str);
                command_position = false;
            }
            Some('$') => {
                lx.bump();
                match lx.peek() {
                    Some('{') => {
                        lx.bump();
                        lx.take_while(|c| c != '}' && c != '\n');
                        lx.bump();
                        lx.push(start, Tok::Type);
                    }
                    Some('(') => {
                        lx.bump();
                        lx.push(start, Tok::Punct);
                        command_position = true;
                    }
                    Some(c) if is_ident_part(c) => {
                        lx.take_while(is_ident_part);
                        lx.push(start, Tok::Type);
                    }
                    _ => {
                        lx.bump();
                        lx.push(start, Tok::Type);
                    }
                }
            }
            Some('<') if lx.at("<<") && !lx.at("<<<") => {
                lx.bump_n(2);
                let strip = lx.peek() == Some('-');
                if strip {
                    lx.bump();
                }
                lx.push(start, Tok::Punct);
                lx.take_while(|c| c == ' ' || c == '\t');
                let quote = matches!(lx.peek(), Some('\'' | '"'));
                if quote {
                    lx.bump();
                }
                let (ds, de) = scan_word(lx);
                if quote {
                    lx.bump();
                }
                lx.push(ds, Tok::Type);
                if de > ds {
                    heredoc = Some((ds, de, strip));
                }
            }
            Some(c) if c.is_ascii_digit() => {
                scan_number(lx);
                lx.push(start, Tok::Number);
                command_position = false;
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let word = &src[s..e];
                let keyword = SH_KEYWORDS.binary_search(&word).is_ok();
                let kind = if keyword {
                    Tok::Keyword
                } else if lx.peek() == Some('=') {
                    Tok::Type
                } else if command_position {
                    Tok::Func
                } else {
                    Tok::Ident
                };
                lx.push(s, kind);
                // `if`, `then` and `do` are followed by a command; `for` and
                // `in` are followed by a variable and a word list.
                command_position = keyword && !matches!(word, "for" | "in" | "case" | "select");
            }
            Some(c) => {
                lx.bump();
                lx.push(start, Tok::Punct);
                command_position = matches!(c, '|' | ';' | '&' | '(' | '{');
            }
        }
        force_progress(lx, before);
    }
}

/// Consumes a heredoc body: every line up to and including the one that is the
/// delimiter alone (leading tabs allowed after `<<-`).
fn scan_heredoc(lx: &mut Lexer, delim_start: usize, delim_end: usize, strip: bool) {
    let src = lx.src;
    let delim = &src[delim_start..delim_end];
    while !lx.eof() {
        let line_start = lx.pos;
        lx.eat_line();
        let line = &src[line_start..lx.pos];
        let candidate = if strip {
            line.trim_start_matches(['\t', ' '])
        } else {
            line
        };
        let terminator = candidate == delim;
        lx.bump(); // the newline, if any
        if terminator {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn json(lx: &mut Lexer) {
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None => break,
            Some(c) if c.is_whitespace() => {
                lx.take_while(char::is_whitespace);
                lx.push(start, Tok::Plain);
            }
            Some('"') => {
                scan_quoted(lx, '"', true, false);
                // A string followed by `:` is a key, and reading a key as a
                // value is most of what makes JSON hard to skim.
                let key = lx
                    .rest()
                    .chars()
                    .find(|c| !c.is_whitespace())
                    .is_some_and(|c| c == ':');
                lx.push(start, if key { Tok::Type } else { Tok::Str });
            }
            Some(c)
                if c.is_ascii_digit()
                    || (c == '-' && lx.peek_nth(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                lx.bump();
                scan_number(lx);
                lx.push(start, Tok::Number);
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let kind = match &src[s..e] {
                    "true" | "false" | "null" => Tok::Keyword,
                    _ => Tok::Ident,
                };
                lx.push(s, kind);
            }
            Some(_) => {
                lx.bump();
                lx.push(start, Tok::Punct);
            }
        }
        force_progress(lx, before);
    }
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

fn toml(lx: &mut Lexer) {
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None => break,
            Some(c) if c.is_whitespace() => {
                lx.take_while(char::is_whitespace);
                lx.push(start, Tok::Plain);
            }
            Some('#') => {
                lx.eat_line();
                lx.push(start, Tok::Comment);
            }
            Some('[') if lx.at_line_start() => {
                lx.take_while(|c| c != ']' && c != '\n');
                lx.bump();
                if lx.peek() == Some(']') {
                    lx.bump();
                }
                lx.push(start, Tok::Type);
            }
            Some('"' | '\'') => {
                toml_string(lx);
                lx.push(start, Tok::Str);
            }
            Some(c)
                if c.is_ascii_digit()
                    || (c == '-' && lx.peek_nth(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                lx.bump();
                scan_number(lx);
                lx.push(start, Tok::Number);
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let word = &src[s..e];
                let key = lx
                    .rest()
                    .chars()
                    .find(|c| !matches!(c, ' ' | '\t'))
                    .is_some_and(|c| c == '=' || c == '.');
                let kind = match word {
                    "true" | "false" => Tok::Keyword,
                    _ if key => Tok::Type,
                    _ => Tok::Ident,
                };
                lx.push(s, kind);
            }
            Some(_) => {
                lx.bump();
                lx.push(start, Tok::Punct);
            }
        }
        force_progress(lx, before);
    }
}

fn toml_string(lx: &mut Lexer) {
    let quote = lx.peek().unwrap_or('"');
    let triple: String = std::iter::repeat_n(quote, 3).collect();
    if lx.at(&triple) {
        lx.bump_n(3);
        scan_until(lx, &triple, quote == '"');
        return;
    }
    // A literal (single-quoted) string takes no escapes.
    scan_quoted(lx, quote, quote == '"', false);
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

/// YAML is scanned a line at a time, because that is the unit its structure
/// lives in — including block scalars (`key: |`), whose body is every following
/// line indented deeper than the key.
fn yaml(lx: &mut Lexer) {
    let mut block: Option<usize> = None;
    while !lx.eof() {
        let before = lx.pos;
        let line_start = lx.pos;
        let line = lx.line_ahead();
        let indent = line.len() - line.trim_start().len();
        let blank = line.trim().is_empty();

        if let Some(base) = block {
            if blank || indent > base {
                lx.eat_line();
                lx.push(line_start, Tok::Str);
                consume_newline(lx);
                continue;
            }
            block = None;
        }

        if blank {
            lx.eat_line();
            lx.push(line_start, Tok::Plain);
            consume_newline(lx);
            continue;
        }

        // Indentation, then the optional sequence marker.
        lx.bump_n(line[..indent].chars().count());
        lx.push(line_start, Tok::Plain);
        while lx.peek() == Some('-') && matches!(lx.peek_nth(1), Some(' ') | None) {
            let dash = lx.pos;
            lx.bump_n(2);
            lx.push(dash, Tok::Punct);
        }

        if lx.peek() == Some('#') {
            let c = lx.pos;
            lx.eat_line();
            lx.push(c, Tok::Comment);
            consume_newline(lx);
            continue;
        }

        // A key, if this line has one.
        let key_start = lx.pos;
        if matches!(lx.peek(), Some(c) if is_ident_start(c) || c == '"' || c == '\'') {
            let saved = lx.pos;
            if matches!(lx.peek(), Some('"' | '\'')) {
                let q = lx.peek().unwrap_or('"');
                scan_quoted(lx, q, true, false);
            } else {
                lx.take_while(|c| c != ':' && c != '\n' && c != '#');
            }
            if lx.peek() == Some(':') {
                lx.push(key_start, Tok::Type);
                let colon = lx.pos;
                lx.bump();
                lx.push(colon, Tok::Punct);
            } else {
                lx.pos = saved;
            }
        }
        yaml_value(lx, &mut block, indent);
        consume_newline(lx);
        force_progress(lx, before);
    }
}

fn consume_newline(lx: &mut Lexer) {
    if lx.peek() == Some('\n') {
        let start = lx.pos;
        lx.bump();
        lx.push(start, Tok::Plain);
    }
}

/// Everything after `key:` on one line.
fn yaml_value(lx: &mut Lexer, block: &mut Option<usize>, indent: usize) {
    loop {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None | Some('\n') => return,
            Some(c) if c.is_whitespace() => {
                lx.take_while(|c| c.is_whitespace() && c != '\n');
                lx.push(start, Tok::Plain);
            }
            Some('#') => {
                lx.eat_line();
                lx.push(start, Tok::Comment);
                return;
            }
            // `|` or `>` (with any chomping indicator) opens a block scalar:
            // every following line indented deeper than this key is its text.
            Some('|' | '>') => {
                lx.bump();
                lx.take_while(|c| c != '\n');
                lx.push(start, Tok::Punct);
                *block = Some(indent);
                return;
            }
            Some('"' | '\'') => {
                let q = lx.peek().unwrap_or('"');
                scan_quoted(lx, q, true, false);
                lx.push(start, Tok::Str);
            }
            Some('&' | '*' | '!') => {
                lx.bump();
                lx.take_while(is_ident_part);
                lx.push(start, Tok::Type);
            }
            Some(c)
                if c.is_ascii_digit()
                    || (c == '-' && lx.peek_nth(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                lx.bump();
                scan_number(lx);
                lx.push(start, Tok::Number);
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let kind = match src[s..e].to_ascii_lowercase().as_str() {
                    "true" | "false" | "null" | "yes" | "no" | "on" | "off" => Tok::Keyword,
                    _ => Tok::Ident,
                };
                lx.push(s, kind);
            }
            Some(_) => {
                lx.bump();
                lx.push(start, Tok::Punct);
            }
        }
        force_progress(lx, before);
    }
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

fn sql(lx: &mut Lexer) {
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None => break,
            Some(c) if c.is_whitespace() => {
                lx.take_while(char::is_whitespace);
                lx.push(start, Tok::Plain);
            }
            Some('-') if lx.at("--") => {
                lx.eat_line();
                lx.push(start, Tok::Comment);
            }
            Some('/') if lx.at("/*") => {
                scan_block_comment(lx, "/*", "*/", false);
                lx.push(start, Tok::Comment);
            }
            Some('\'') => {
                // `''` inside a literal is an escaped quote, not a close
                // followed by an open.
                lx.bump();
                loop {
                    match lx.peek() {
                        None => break,
                        Some('\'') if lx.peek_nth(1) == Some('\'') => lx.bump_n(2),
                        Some('\'') => {
                            lx.bump();
                            break;
                        }
                        _ => {
                            lx.bump();
                        }
                    }
                }
                lx.push(start, Tok::Str);
            }
            Some('"' | '`') => {
                let q = lx.peek().unwrap_or('"');
                scan_quoted(lx, q, false, false);
                lx.push(start, Tok::Type);
            }
            Some(c) if c.is_ascii_digit() => {
                scan_number(lx);
                lx.push(start, Tok::Number);
            }
            Some(c) if is_ident_start(c) => {
                let src = lx.src;
                let (s, e) = scan_word(lx);
                let word = &src[s..e];
                let kind = if SQL_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word)) {
                    Tok::Keyword
                } else if SQL_TYPES.iter().any(|k| k.eq_ignore_ascii_case(word)) {
                    Tok::Type
                } else if next_is_call(lx) {
                    Tok::Func
                } else {
                    Tok::Ident
                };
                lx.push(s, kind);
            }
            Some(_) => {
                lx.bump();
                lx.push(start, Tok::Punct);
            }
        }
        force_progress(lx, before);
    }
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

fn html(lx: &mut Lexer) {
    let mut in_tag = false;
    while !lx.eof() {
        let before = lx.pos;
        let start = lx.pos;
        if !in_tag {
            if lx.at("<!--") {
                lx.bump_n(4);
                scan_until(lx, "-->", false);
                lx.push(start, Tok::Comment);
            } else if lx.at("<!") || lx.at("<?") {
                lx.take_while(|c| c != '>');
                lx.bump();
                lx.push(start, Tok::Keyword);
            } else if lx.at("<") {
                lx.bump();
                if lx.peek() == Some('/') {
                    lx.bump();
                }
                lx.push(start, Tok::Punct);
                let name = lx.pos;
                lx.take_while(|c| is_ident_part(c) || c == '-' || c == ':');
                lx.push(name, Tok::Keyword);
                in_tag = true;
            } else {
                lx.take_while(|c| c != '<');
                lx.push(start, Tok::Plain);
            }
        } else {
            match lx.peek() {
                None => break,
                Some(c) if c.is_whitespace() => {
                    lx.take_while(char::is_whitespace);
                    lx.push(start, Tok::Plain);
                }
                Some('>') => {
                    lx.bump();
                    lx.push(start, Tok::Punct);
                    in_tag = false;
                }
                Some('"' | '\'') => {
                    let q = lx.peek().unwrap_or('"');
                    scan_quoted(lx, q, false, true);
                    lx.push(start, Tok::Str);
                }
                Some(c) if is_ident_start(c) => {
                    lx.take_while(|c| is_ident_part(c) || c == '-' || c == ':');
                    lx.push(start, Tok::Type);
                }
                Some(_) => {
                    lx.bump();
                    lx.push(start, Tok::Punct);
                }
            }
        }
        force_progress(lx, before);
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// Markdown inside markdown: a transcript quotes documents at least as often as
/// it quotes code, and a nested fence has to stay a fence.
fn markdown(lx: &mut Lexer) {
    let mut fence: Option<(char, usize)> = None;
    while !lx.eof() {
        let before = lx.pos;
        let line_start = lx.pos;
        let line = lx.line_ahead();
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if let Some((ch, len)) = fence {
            let closing = trimmed.starts_with(&std::iter::repeat_n(ch, len).collect::<String>());
            lx.eat_line();
            lx.push(line_start, if closing { Tok::Punct } else { Tok::Str });
            if closing {
                fence = None;
            }
            consume_newline(lx);
            continue;
        }

        if let Some(ch) = ['`', '~']
            .into_iter()
            .find(|&c| trimmed.starts_with(&std::iter::repeat_n(c, 3).collect::<String>()))
        {
            let len = trimmed.chars().take_while(|&c| c == ch).count();
            lx.eat_line();
            lx.push(line_start, Tok::Punct);
            fence = Some((ch, len));
            consume_newline(lx);
            continue;
        }

        if trimmed.starts_with('#') {
            lx.eat_line();
            lx.push(line_start, Tok::Keyword);
            consume_newline(lx);
            continue;
        }
        if trimmed.starts_with('>') {
            lx.eat_line();
            lx.push(line_start, Tok::Comment);
            consume_newline(lx);
            continue;
        }
        if !trimmed.is_empty()
            && trimmed.len() >= 3
            && trimmed.chars().all(|c| c == '-' || c == '=' || c == '*')
        {
            lx.eat_line();
            lx.push(line_start, Tok::Punct);
            consume_newline(lx);
            continue;
        }

        lx.bump_n(line[..indent].chars().count());
        lx.push(line_start, Tok::Plain);
        let marker = lx.pos;
        if matches!(lx.peek(), Some('-' | '*' | '+')) && lx.peek_nth(1) == Some(' ') {
            lx.bump_n(2);
            lx.push(marker, Tok::Punct);
        } else if lx.peek().is_some_and(|c| c.is_ascii_digit()) {
            let saved = lx.pos;
            lx.take_while(|c| c.is_ascii_digit());
            if matches!(lx.peek(), Some('.' | ')')) && lx.peek_nth(1) == Some(' ') {
                lx.bump_n(2);
                lx.push(marker, Tok::Punct);
            } else {
                lx.pos = saved;
            }
        }
        md_inline(lx);
        consume_newline(lx);
        force_progress(lx, before);
    }
}

/// Inline markdown to the end of the current line.
fn md_inline(lx: &mut Lexer) {
    loop {
        let before = lx.pos;
        let start = lx.pos;
        match lx.peek() {
            None | Some('\n') => return,
            Some('`') => {
                let ticks = lx.rest().chars().take_while(|&c| c == '`').count();
                lx.bump_n(ticks);
                let close: String = std::iter::repeat_n('`', ticks).collect();
                while !lx.eof() && lx.peek() != Some('\n') && !lx.at(&close) {
                    lx.bump();
                }
                if lx.at(&close) {
                    lx.bump_n(ticks);
                }
                lx.push(start, Tok::Str);
            }
            Some('*' | '_' | '~') => {
                lx.take_while(|c| matches!(c, '*' | '_' | '~'));
                lx.push(start, Tok::Punct);
            }
            Some('[' | ']' | '(' | ')') => {
                let bracket = lx.bump();
                lx.push(start, Tok::Punct);
                // `](url)` — the destination reads as a link, not as prose.
                if bracket == Some('(') && lx.prev_is_link_open() {
                    let url = lx.pos;
                    lx.take_while(|c| c != ')' && c != '\n');
                    lx.push(url, Tok::Func);
                }
            }
            Some(_) => {
                lx.take_while(|c| {
                    !matches!(c, '`' | '*' | '_' | '~' | '[' | ']' | '(' | ')' | '\n')
                });
                lx.push(start, Tok::Plain);
            }
        }
        force_progress(lx, before);
    }
}

impl Lexer<'_> {
    /// Whether the `(` just consumed follows a `]` — i.e. opens a link
    /// destination rather than a parenthetical.
    fn prev_is_link_open(&self) -> bool {
        let before = &self.src[..self.pos.saturating_sub(1)];
        before.ends_with(']')
    }
}

// ---------------------------------------------------------------------------
// Word tables. Sorted, because lookup is a binary search — enforced by
// `tables_are_sorted` rather than by care.
// ---------------------------------------------------------------------------

const RUST_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "become",
    "box",
    "break",
    "const",
    "continue",
    "crate",
    "do",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "macro",
    "macro_rules",
    "match",
    "mod",
    "move",
    "mut",
    "override",
    "priv",
    "pub",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "union",
    "unsafe",
    "unsized",
    "use",
    "virtual",
    "where",
    "while",
    "yield",
];

const RUST_TYPES: &[&str] = &[
    "Arc", "BTreeMap", "BTreeSet", "Box", "Cow", "HashMap", "HashSet", "Mutex", "Option", "Path",
    "PathBuf", "Rc", "RefCell", "Result", "Self", "String", "Vec", "bool", "char", "f32", "f64",
    "i128", "i16", "i32", "i64", "i8", "isize", "str", "u128", "u16", "u32", "u64", "u8", "usize",
];

const JS_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "satisfies",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const JS_TYPES: &[&str] = &[
    "Array", "Boolean", "Map", "Number", "Object", "Promise", "Set", "String", "any", "bigint",
    "boolean", "never", "number", "object", "string", "symbol", "unknown",
];

const GO_KEYWORDS: &[&str] = &[
    "append",
    "break",
    "cap",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "iota",
    "len",
    "make",
    "map",
    "new",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];

const GO_TYPES: &[&str] = &[
    "any",
    "bool",
    "byte",
    "complex128",
    "complex64",
    "error",
    "float32",
    "float64",
    "int",
    "int16",
    "int32",
    "int64",
    "int8",
    "rune",
    "string",
    "uint",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "uintptr",
];

const PY_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def", "del",
    "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "self", "try", "while",
    "with", "yield",
];

const PY_TYPES: &[&str] = &[
    "Any",
    "Dict",
    "Exception",
    "List",
    "None",
    "Optional",
    "True",
    "Tuple",
    "bool",
    "bytes",
    "dict",
    "float",
    "int",
    "list",
    "object",
    "set",
    "str",
    "tuple",
];

const SH_KEYWORDS: &[&str] = &[
    "case", "declare", "do", "done", "elif", "else", "esac", "eval", "exec", "exit", "export",
    "fi", "for", "function", "if", "in", "local", "readonly", "return", "select", "set", "shift",
    "source", "then", "trap", "unset", "until", "while",
];

const SQL_KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "commit",
    "create",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "returning",
    "right",
    "rollback",
    "select",
    "set",
    "table",
    "then",
    "transaction",
    "union",
    "unique",
    "update",
    "values",
    "view",
    "when",
    "where",
    "with",
];

const SQL_TYPES: &[&str] = &[
    "bigint",
    "blob",
    "boolean",
    "char",
    "date",
    "decimal",
    "double",
    "float",
    "int",
    "integer",
    "json",
    "jsonb",
    "numeric",
    "real",
    "serial",
    "smallint",
    "text",
    "timestamp",
    "timestamptz",
    "uuid",
    "varchar",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::ansi()
    }

    /// The rendered text of every line, joined — what the user reads.
    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The token kind covering `needle`'s first occurrence in `src`.
    fn kind_of(src: &str, lang: Lang, needle: &str) -> Tok {
        let at = src.find(needle).expect("needle not in source");
        let tokens = tokenize(src, lang);
        tokens
            .iter()
            .find(|&&(s, e, _)| s <= at && at < e)
            .map(|&(_, _, k)| k)
            .unwrap_or(Tok::Plain)
    }

    fn kinds(src: &str, lang: Lang) -> Vec<(&str, Tok)> {
        tokenize(src, lang)
            .into_iter()
            .map(|(s, e, k)| (&src[s..e], k))
            .collect()
    }

    #[test]
    fn tables_are_sorted_and_deduplicated() {
        // Lookup is a binary search; an out-of-order entry is silently invisible.
        for (name, table) in [
            ("rust kw", RUST_KEYWORDS),
            ("rust ty", RUST_TYPES),
            ("js kw", JS_KEYWORDS),
            ("js ty", JS_TYPES),
            ("go kw", GO_KEYWORDS),
            ("go ty", GO_TYPES),
            ("py kw", PY_KEYWORDS),
            ("py ty", PY_TYPES),
            ("sh kw", SH_KEYWORDS),
        ] {
            for pair in table.windows(2) {
                assert!(pair[0] < pair[1], "{name}: {:?} >= {:?}", pair[0], pair[1]);
            }
        }
    }

    #[test]
    fn an_unknown_or_absent_language_is_left_alone() {
        assert!(highlight("whatever", "", &theme()).is_none());
        assert!(highlight("whatever", "brainfuck", &theme()).is_none());
        assert!(highlight("whatever", "text", &theme()).is_none());
        // ...and a known one, however it was spelled, is not.
        for info in ["rust", "RS", "ts", "```yml", "js {1,3}", "sql,ignore"] {
            assert!(highlight("x", info, &theme()).is_some(), "{info}");
        }
    }

    #[test]
    fn output_has_exactly_one_line_per_source_line() {
        for src in [
            "one",
            "one\ntwo",
            "one\n\nthree",
            "trailing\n",
            "\nleading",
            "",
        ] {
            for lang in ["rust", "python", "yaml", "toml", "json", "bash", "md"] {
                let lines = highlight(src, lang, &theme()).unwrap();
                assert_eq!(
                    lines.len(),
                    src.split('\n').count(),
                    "{lang} disagreed on {src:?}"
                );
                assert_eq!(plain(&lines), src, "{lang} altered {src:?}");
            }
        }
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        let src = r#"let s = "// not a comment"; // real"#;
        assert_eq!(kind_of(src, Lang::Rust, "// not"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Rust, "// real"), Tok::Comment);
        // ...and the quote inside a comment does not open a string.
        let src = "// it's fine\nlet x = 1;";
        assert_eq!(kind_of(src, Lang::Rust, "it's"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Rust, "let"), Tok::Keyword);
        assert_eq!(kind_of(src, Lang::Rust, "1"), Tok::Number);
        // Shell: `#` inside a string is not a comment either.
        let src = "echo \"# heading\" # note";
        assert_eq!(kind_of(src, Lang::Shell, "# heading"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Shell, "# note"), Tok::Comment);
    }

    #[test]
    fn block_comments_nest_and_survive_never_closing() {
        let src = "/* /* inner */ still */ let x = 1;";
        assert_eq!(kind_of(src, Lang::Rust, "still"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Rust, "let"), Tok::Keyword);
        // An unterminated nested comment runs to the end — and terminates.
        let src = "/* /* */\nfn main() {}";
        assert_eq!(kind_of(src, Lang::Rust, "fn main"), Tok::Comment);
        // Go does not nest, so the first `*/` closes.
        let src = "/* /* */ func main() {}";
        assert_eq!(kind_of(src, Lang::Go, "func"), Tok::Keyword);
        // Unterminated, unnested.
        let src = "/* forever\nand ever";
        assert_eq!(kind_of(src, Lang::Go, "ever"), Tok::Comment);
    }

    #[test]
    fn rust_raw_strings_and_lifetimes_and_char_literals() {
        let src =
            r####"let s = r#"he said "hi" // ok"#; let t = 'a'; fn f<'de>(x: &'de str) {}"####;
        assert_eq!(kind_of(src, Lang::Rust, r#""hi""#), Tok::Str);
        assert_eq!(kind_of(src, Lang::Rust, "// ok"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Rust, "'a'"), Tok::Str);
        // A lifetime is not an unterminated char literal: what follows stays code.
        assert_eq!(kind_of(src, Lang::Rust, "'de"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Rust, "str"), Tok::Type);
        // An identifier that merely starts with `r` or `b` is not a string.
        assert_eq!(kind_of("let rows = 1;", Lang::Rust, "rows"), Tok::Ident);
        assert_eq!(kind_of("let bytes = 1;", Lang::Rust, "bytes"), Tok::Ident);
        // An unterminated raw string ends the block instead of hanging.
        let src = "let s = r#\"never closed";
        assert_eq!(kind_of(src, Lang::Rust, "never"), Tok::Str);
    }

    #[test]
    fn rust_names_functions_macros_and_types() {
        let src = "fn build(cfg: Config) -> Vec<u8> { println!(\"x\"); helper(cfg) }";
        assert_eq!(kind_of(src, Lang::Rust, "build"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Rust, "Config"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Rust, "Vec"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Rust, "u8"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Rust, "println!"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Rust, "helper"), Tok::Func);
    }

    #[test]
    fn python_triple_quotes_span_lines_and_keep_their_colour() {
        let src = "x = 1\ns = \"\"\"line one\n# not a comment\nline three\"\"\"\ny = 2";
        let lines = highlight(src, "python", &theme()).unwrap();
        assert_eq!(lines.len(), 5);
        // The second and third lines of the string are still string-coloured.
        let string_style = style_for(Tok::Str, &theme());
        for (row, line) in lines.iter().enumerate().take(4).skip(2) {
            assert!(
                line.spans.iter().all(|s| s.style == string_style),
                "row {row} lost the string style: {line:?}"
            );
        }
        assert_eq!(kind_of(src, Lang::Python, "# not"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Python, "y ="), Tok::Ident);
    }

    #[test]
    fn python_string_prefixes_and_decorators() {
        let src = "@cache\ndef f(p: str) -> None:\n    return rb'\\x00' + f\"{p}\"";
        assert_eq!(kind_of(src, Lang::Python, "@cache"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Python, "def"), Tok::Keyword);
        assert_eq!(kind_of(src, Lang::Python, "f("), Tok::Func);
        assert_eq!(kind_of(src, Lang::Python, "rb'"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Python, "f\""), Tok::Str);
        assert_eq!(kind_of(src, Lang::Python, "None"), Tok::Type);
    }

    #[test]
    fn js_template_literals_highlight_their_holes() {
        let src = "const s = `n = ${count + 1} done`;";
        assert_eq!(kind_of(src, Lang::Js, "n = "), Tok::Str);
        assert_eq!(kind_of(src, Lang::Js, "${"), Tok::Punct);
        assert_eq!(kind_of(src, Lang::Js, "count"), Tok::Ident);
        assert_eq!(kind_of(src, Lang::Js, "1"), Tok::Number);
        assert_eq!(kind_of(src, Lang::Js, " done"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Js, "const"), Tok::Keyword);
        // A template spanning lines stays a string on the second line.
        let src = "const s = `a\nb`;";
        assert_eq!(kind_of(src, Lang::Js, "b"), Tok::Str);
        // Nested interpolation terminates rather than recursing forever.
        let src = "`${`${`${`${`${`${`${`${`${x}`}`}`}`}`}`}`}`}`}`";
        let lines = highlight(src, "js", &theme()).unwrap();
        assert_eq!(plain(&lines), src);
    }

    #[test]
    fn shell_heredocs_and_quotes() {
        let src = "cat <<'EOF' > out\n# literal $HOME\nEOF\necho done";
        assert_eq!(kind_of(src, Lang::Shell, "cat"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Shell, "# literal"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Shell, "EOF\necho"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Shell, "echo"), Tok::Func);
        // An indented terminator only closes a `<<-` heredoc.
        let src = "cat <<-EOF\nbody\n\tEOF\nls";
        assert_eq!(kind_of(src, Lang::Shell, "body"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Shell, "ls"), Tok::Func);
        // A heredoc that never terminates stops at the end of the block.
        let src = "cat <<EOF\nbody\nmore";
        assert_eq!(kind_of(src, Lang::Shell, "more"), Tok::Str);
        // Single quotes take no escapes.
        let src = r"echo 'it\'; echo $HOME";
        assert_eq!(kind_of(src, Lang::Shell, r"'it\'"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Shell, "$HOME"), Tok::Type);
    }

    #[test]
    fn json_keys_read_differently_from_values() {
        let src = "{\"name\": \"smith\", \"n\": 3, \"ok\": true}";
        assert_eq!(kind_of(src, Lang::Json, "\"name\""), Tok::Type);
        assert_eq!(kind_of(src, Lang::Json, "\"smith\""), Tok::Str);
        assert_eq!(kind_of(src, Lang::Json, "3"), Tok::Number);
        assert_eq!(kind_of(src, Lang::Json, "true"), Tok::Keyword);
    }

    #[test]
    fn toml_yaml_and_sql_basics() {
        let src = "[search]\nbackend = \"searxng\" # pinned\nport = 8080";
        assert_eq!(kind_of(src, Lang::Toml, "[search]"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Toml, "backend"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Toml, "\"searxng\""), Tok::Str);
        assert_eq!(kind_of(src, Lang::Toml, "# pinned"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Toml, "8080"), Tok::Number);

        let src = "name: smith\nversion: 1\nscript: |\n  echo hi\n  echo bye\ndone: true";
        assert_eq!(kind_of(src, Lang::Yaml, "name"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Yaml, "1"), Tok::Number);
        assert_eq!(kind_of(src, Lang::Yaml, "echo hi"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Yaml, "echo bye"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Yaml, "done"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Yaml, "true"), Tok::Keyword);

        let src = "SELECT count(*) FROM t -- tail\nWHERE s = 'it''s' AND n > 2";
        assert_eq!(kind_of(src, Lang::Sql, "SELECT"), Tok::Keyword);
        assert_eq!(kind_of(src, Lang::Sql, "count"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Sql, "-- tail"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Sql, "'it''s'"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Sql, "AND"), Tok::Keyword);
    }

    #[test]
    fn html_and_markdown_basics() {
        let src = "<!-- note -->\n<a href=\"/x\" class='y'>text</a>";
        assert_eq!(kind_of(src, Lang::Html, "<!-- note"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Html, "href"), Tok::Type);
        assert_eq!(kind_of(src, Lang::Html, "\"/x\""), Tok::Str);
        assert_eq!(kind_of(src, Lang::Html, "'y'"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Html, "text"), Tok::Plain);

        let src = "# Title\n\n- see [docs](http://x) and `code`\n\n```rs\nfn f() {}\n```\nafter";
        assert_eq!(kind_of(src, Lang::Markdown, "# Title"), Tok::Keyword);
        assert_eq!(kind_of(src, Lang::Markdown, "http://x"), Tok::Func);
        assert_eq!(kind_of(src, Lang::Markdown, "`code`"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Markdown, "fn f() {}"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Markdown, "after"), Tok::Plain);
    }

    #[test]
    fn multibyte_text_is_neither_split_nor_panicked_on() {
        // The repo has already shipped one panic from slicing a &str mid-char.
        let src = "// café ☕ — ok\nlet s = \"naïve 🚀 façade\";\nlet ç = 'é';";
        let lines = highlight(src, "rust", &theme()).unwrap();
        assert_eq!(plain(&lines), src);
        assert_eq!(kind_of(src, Lang::Rust, "café"), Tok::Comment);
        assert_eq!(kind_of(src, Lang::Rust, "🚀"), Tok::Str);
        assert_eq!(kind_of(src, Lang::Rust, "'é'"), Tok::Str);
        // Every language, over the same multibyte soup, must round-trip.
        let soup = "🚀 café — ç\n\"日本語\" # ✓\n<é> `ü` ${ø}\n";
        for lang in [
            "rust", "python", "js", "json", "toml", "yaml", "bash", "go", "sql", "html", "md",
        ] {
            let lines = highlight(soup, lang, &theme()).unwrap();
            assert_eq!(plain(&lines), soup, "{lang} mangled multibyte input");
        }
    }

    #[test]
    fn highlighting_introduces_no_glyph_of_its_own() {
        // Acceptance criterion #7: nothing non-ASCII may appear under an ASCII
        // theme. Spans are slices of the source, so ASCII in means ASCII out.
        let ascii = Theme::ansi().ascii_glyphs();
        let src = "fn main() {\n    let s = \"hi\"; // ok\n}";
        for lang in ["rust", "python", "js", "json", "toml", "yaml", "bash", "md"] {
            let lines = highlight(src, lang, &ascii).unwrap();
            for line in &lines {
                for span in &line.spans {
                    assert!(
                        span.content.is_ascii(),
                        "{lang} produced non-ASCII: {:?}",
                        span.content
                    );
                }
            }
        }
    }

    #[test]
    fn a_large_block_is_linear_and_finishes() {
        let mut src = String::new();
        for i in 0..5000 {
            src.push_str(&format!(
                "    let value_{i} = compute(\"text {i}\", {i}); // step {i}\n"
            ));
        }
        let started = std::time::Instant::now();
        let lines = highlight(&src, "rust", &theme()).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(lines.len(), 5001);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "5000 lines took {elapsed:?} — that is not linear"
        );
    }

    #[test]
    fn pathological_input_terminates() {
        // Every one of these used to be a plausible infinite loop or panic.
        let cases = [
            ("rust", "r#####\"unclosed"),
            ("rust", "/*/*/*/*"),
            ("rust", "'"),
            ("rust", "\""),
            ("js", "`${"),
            ("js", "`${${${${"),
            ("python", "'''"),
            ("python", "f'"),
            ("bash", "<<"),
            ("bash", "<<EOF"),
            ("bash", "'"),
            ("yaml", "key: |"),
            ("yaml", ":"),
            ("yaml", "-"),
            ("toml", "[unclosed"),
            ("toml", "\"\"\""),
            ("sql", "'"),
            ("html", "<"),
            ("html", "<!--"),
            ("md", "```"),
            ("md", "["),
            ("md", "1."),
        ];
        for (lang, src) in cases {
            let lines = highlight(src, lang, &theme()).expect(lang);
            assert_eq!(plain(&lines), src, "{lang} altered {src:?}");
        }
    }

    #[test]
    fn tokens_cover_the_source_exactly_once() {
        let src = "fn f() { let x = \"s\"; } // end";
        let mut end = 0;
        for (s, e, _) in tokenize(src, Lang::Rust) {
            assert!(s >= end, "overlapping tokens at {s}");
            assert!(e > s, "empty token at {s}");
            end = e;
        }
        assert_eq!(end, src.len());
        // And the whole text survives regardless.
        assert_eq!(
            kinds(src, Lang::Rust)
                .iter()
                .map(|(t, _)| *t)
                .collect::<String>(),
            src
        );
    }
}
