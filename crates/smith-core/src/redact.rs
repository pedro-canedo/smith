//! Keeps known secrets out of tool output.
//!
//! A single `run_bash {"command": "env"}` — or `cat ~/.smith/config.toml`, or
//! a build script that echoes its environment — used to put the raw API key
//! into three places at once: the visible transcript, the `messages` table in
//! SQLite, and the very next request sent to the provider. The third is the
//! worst: the key gets handed to a third party as conversation content.
//!
//! This is deliberately literal matching against the keys smith actually
//! loaded, not a general secret scanner. It is the 90% of the value for 5% of
//! the effort, it has no false positives by construction, and it needs no
//! dependency. Pattern-based detection (`sk-`, `ghp_`, `AKIA`, `Bearer`, PEM
//! headers) is a worthwhile follow-up for secrets smith never saw, but it
//! belongs on top of this rather than instead of it.

use std::borrow::Cow;

/// Shortest string worth treating as a secret. Anything below this is far
/// more likely to be a placeholder (`""`, `"none"`, `"test"`) than a real
/// credential, and redacting it would corrupt unrelated output.
const MIN_SECRET_LEN: usize = 12;

pub const REDACTED: &str = "[redacted]";

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Sorted longest-first so an overlapping pair (a key and a prefix of it)
    /// can't leave the tail of the longer one exposed.
    literals: Vec<String>,
}

impl Redactor {
    pub fn new(literals: impl IntoIterator<Item = String>) -> Self {
        let mut literals: Vec<String> = literals
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| s.chars().count() >= MIN_SECRET_LEN)
            .collect();
        literals.sort_by_key(|s| std::cmp::Reverse(s.len()));
        literals.dedup();
        Self { literals }
    }

    pub fn is_empty(&self) -> bool {
        self.literals.is_empty()
    }

    /// Borrows unchanged text — the overwhelmingly common case, since most
    /// tool output contains no secret at all.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.literals.iter().all(|s| !text.contains(s.as_str())) {
            return Cow::Borrowed(text);
        }
        let mut out = text.to_string();
        for secret in &self.literals {
            out = out.replace(secret.as_str(), REDACTED);
        }
        Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-abcdefghijklmnop";

    #[test]
    fn replaces_a_known_key_anywhere_in_the_text() {
        let r = Redactor::new([KEY.to_string()]);
        let input = format!("ANTHROPIC_API_KEY={KEY}\nOTHER=1");
        let out = r.redact(&input);
        assert!(!out.contains(KEY), "key survived: {out}");
        assert!(out.contains(REDACTED));
        assert!(out.contains("OTHER=1"), "unrelated output must survive");
    }

    #[test]
    fn leaves_clean_text_untouched_and_unallocated() {
        let r = Redactor::new([KEY.to_string()]);
        assert!(matches!(r.redact("nothing to see"), Cow::Borrowed(_)));
    }

    #[test]
    fn ignores_short_values_that_would_corrupt_output() {
        // An unset or placeholder key must not turn every "test" in a build
        // log into [redacted].
        let r = Redactor::new(["".to_string(), "test".to_string(), "none".to_string()]);
        assert!(r.is_empty());
        assert_eq!(r.redact("running test suite"), "running test suite");
    }

    #[test]
    fn redacts_every_occurrence_not_just_the_first() {
        let r = Redactor::new([KEY.to_string()]);
        let input = format!("{KEY} and again {KEY}");
        let out = r.redact(&input);
        assert!(!out.contains(KEY), "got: {out}");
        assert_eq!(out.matches(REDACTED).count(), 2);
    }

    /// A shorter key that is a prefix of a longer one must not be replaced
    /// first, or the tail of the longer key would be left in the clear.
    #[test]
    fn overlapping_secrets_are_fully_removed() {
        let short = "sk-abcdefghijkl";
        let long = format!("{short}-mnopqrstuv");
        let r = Redactor::new([short.to_string(), long.clone()]);
        let input = format!("key={long}");
        let out = r.redact(&input);
        assert!(!out.contains(short), "leaked a fragment: {out}");
    }

    #[test]
    fn handles_multiple_distinct_secrets() {
        let a = "sk-ant-aaaaaaaaaaaaaaaa";
        let b = "sk-openai-bbbbbbbbbbbbbb";
        let r = Redactor::new([a.to_string(), b.to_string()]);
        let input = format!("{a}\n{b}");
        let out = r.redact(&input);
        assert!(!out.contains(a) && !out.contains(b), "got: {out}");
    }
}
