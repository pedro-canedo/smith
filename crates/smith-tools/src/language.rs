//! Guessing the language a search query is written in.
//!
//! Exists because of a measured failure: Bing's RSS endpoint answers a
//! Portuguese query under `setmkt=en-US` with ten well-formed results about
//! nothing (American Idol, job boards, outlet stores — 3 runs out of 3), and
//! the same query under `setmkt=pt-BR` with the right answer, also 3 out of 3.
//! The market has to match the *query's* language, and the machine locale is
//! a bad proxy for it: WSL2 ships `LANG=C.UTF-8`, which names no market at
//! all, and an agent's queries mix languages within one session anyway.
//!
//! The detector is deliberately a couple of word lists, not a library. It
//! only has to answer one question — "is this one of the handful of languages
//! whose market we would pick over the default?" — and a wrong `None` costs
//! exactly what today's behaviour always costs; a wrong `Some` is what the
//! scoring rules below are shaped to avoid.

/// A language the detector can vouch for, mapped to the search parameters
/// that language wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryLanguage {
    Portuguese,
    Spanish,
}

impl QueryLanguage {
    /// The Bing market matching the query's language.
    ///
    /// `pt` maps to `pt-BR` rather than `pt-PT` on population and on the
    /// measured case; a Portuguese user gets Brazilian-market results over
    /// poisoned ones, which is strictly an improvement.
    pub(crate) fn bing_market(self) -> &'static str {
        match self {
            QueryLanguage::Portuguese => "pt-BR",
            QueryLanguage::Spanish => "es-ES",
        }
    }

    /// `hl`, `gl` and `ceid` for Google News' RSS search endpoint.
    pub(crate) fn google_news_params(self) -> (&'static str, &'static str, &'static str) {
        match self {
            QueryLanguage::Portuguese => ("pt-BR", "BR", "BR:pt-419"),
            QueryLanguage::Spanish => ("es-419", "MX", "MX:es-419"),
        }
    }
}

/// Words that identify a language when they appear in a query.
///
/// Function words only — articles, prepositions, question words — because
/// content words travel between languages ("mega", "internet", "chat").
/// Words shared with the *other* listed language (de, para, que, como…)
/// are deliberately absent from both lists: a shared word scores for
/// neither, so the two languages cannot smear into each other.
const PORTUGUESE_WORDS: &[&str] = &[
    "da",
    "do",
    "das",
    "dos",
    "na",
    "no",
    "nas",
    "nos",
    "em",
    "um",
    "uma",
    "não",
    "são",
    "com",
    "ao",
    "aos",
    "à",
    "às",
    "é",
    "ou",
    "os",
    "mais",
    "já",
    "também",
    "qual",
    "quais",
    "quando",
    "onde",
    "quem",
    "sobre",
    "hoje",
    "ontem",
    "último",
    "últimos",
    "última",
    "últimas",
    "resultado",
    "resultados",
    "jogos",
    "notícias",
    "melhores",
    "preço",
    "brasileiro",
];
const SPANISH_WORDS: &[&str] = &[
    "el",
    "la",
    "los",
    "las",
    "una",
    "uno",
    "unos",
    "unas",
    "del",
    "al",
    "es",
    "en",
    "con",
    "no",
    "más",
    "también",
    "cuál",
    "cuáles",
    "cuándo",
    "dónde",
    "quién",
    "hoy",
    "ayer",
    "último",
    "últimos",
    "última",
    "últimas",
    "resultado",
    "resultados",
    "mejores",
    "noticias",
    "precio",
    "y",
    "pero",
    "muy",
];

/// Characters that essentially only one of the listed languages uses.
/// `ã`/`õ` are Portuguese; `ñ` and the inverted marks are Spanish. The shared
/// Romance accents (á, é, í, ó, ú, ç) identify "not English" but not which,
/// so they count for nothing here.
const PORTUGUESE_CHARS: &[char] = &['ã', 'õ'];
const SPANISH_CHARS: &[char] = &['ñ', '¿', '¡'];

/// The language `query` appears to be written in, when the evidence is strong
/// enough to act on.
///
/// Two signals, either sufficient: a character unique to the language, or at
/// least **two** of its marker words (one word is a coincidence — "no" is
/// English too; two function words of the same language is a sentence).
/// Ties go to `None`: acting on a guess is how the market mismatch this
/// module exists to fix would be reintroduced pointing the other way.
pub(crate) fn detect(query: &str) -> Option<QueryLanguage> {
    let lower = query.to_lowercase();

    let pt_chars = lower.chars().any(|c| PORTUGUESE_CHARS.contains(&c));
    let es_chars = lower.chars().any(|c| SPANISH_CHARS.contains(&c));
    match (pt_chars, es_chars) {
        (true, false) => return Some(QueryLanguage::Portuguese),
        (false, true) => return Some(QueryLanguage::Spanish),
        _ => {}
    }

    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && !"áéíóúâêôãõçñü".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    let pt = words
        .iter()
        .filter(|w| PORTUGUESE_WORDS.contains(*w))
        .count();
    let es = words.iter().filter(|w| SPANISH_WORDS.contains(*w)).count();

    if pt >= 2 && pt > es {
        Some(QueryLanguage::Portuguese)
    } else if es >= 2 && es > pt {
        Some(QueryLanguage::Spanish)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_two_queries_that_actually_failed() {
        // Straight from the session that motivated this module.
        assert_eq!(
            detect("últimos resultados Campeonato Brasileiro"),
            Some(QueryLanguage::Portuguese)
        );
        assert_eq!(
            detect("resultado da mega sena"),
            Some(QueryLanguage::Portuguese)
        );
    }

    #[test]
    fn detects_spanish_by_words_and_by_characters() {
        assert_eq!(
            detect("los últimos resultados de la lotería"),
            Some(QueryLanguage::Spanish)
        );
        assert_eq!(detect("¿cómo funciona?"), Some(QueryLanguage::Spanish));
        assert_eq!(detect("año nuevo chino"), Some(QueryLanguage::Spanish));
    }

    #[test]
    fn detects_portuguese_by_its_unique_characters() {
        assert_eq!(
            detect("eleições são paulo"),
            Some(QueryLanguage::Portuguese)
        );
        assert_eq!(detect("cotação do dólar"), Some(QueryLanguage::Portuguese));
    }

    #[test]
    fn english_and_technical_queries_stay_undetected() {
        for q in [
            "ratatui crate docs",
            "rust async trait lifetime error",
            "kubernetes pod restart policy",
            "latest election results",
            // "no" is a Spanish marker but one word is never enough.
            "no module named pip",
        ] {
            assert_eq!(detect(q), None, "{q}");
        }
    }

    #[test]
    fn one_marker_word_is_not_evidence() {
        // "da" alone: could be Portuguese, could be "da Vinci".
        assert_eq!(detect("leonardo da vinci"), None);
    }

    #[test]
    fn a_tie_between_languages_abstains() {
        // "resultados" and "último" sit in both lists; a tie must not guess.
        assert_eq!(detect("resultados último"), None);
    }

    #[test]
    fn markets_and_news_params_are_coherent() {
        assert_eq!(QueryLanguage::Portuguese.bing_market(), "pt-BR");
        assert_eq!(QueryLanguage::Spanish.bing_market(), "es-ES");
        assert_eq!(
            QueryLanguage::Portuguese.google_news_params(),
            ("pt-BR", "BR", "BR:pt-419")
        );
    }
}
