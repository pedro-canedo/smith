//! Rebuilding a tool call a model wrote as plain-text JSON.

use std::collections::HashSet;

use crate::message::ToolDefinition;

/// Scans `text` left to right for the first `{...}` JSON object that is a tool
/// call in disguise, returning its name, arguments, and the (trimmed) text
/// before/after it. Uses a streaming JSON parser to find the object's end
/// rather than hand-rolled brace counting, so it doesn't get confused by
/// braces inside string values.
pub(super) fn find_fallback_tool_call(
    text: &str,
    known_tools: &[ToolDefinition],
) -> Option<(String, serde_json::Value, String, String)> {
    for (i, byte) in text.bytes().enumerate() {
        if byte != b'{' {
            continue;
        }
        let mut de =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        let Some(Ok(value)) = de.next() else {
            continue;
        };
        if let Some((name, arguments)) = parse_tool_call_envelope(&value, known_tools) {
            let end = i + de.byte_offset();
            return Some((
                name,
                arguments,
                text[..i].trim().to_string(),
                text[end..].trim().to_string(),
            ));
        }
    }
    None
}

/// The two envelopes a model may use to ask for a tool in plain text, both
/// gated on naming a *registered* tool — JSON-shaped prose that happens to
/// have a `name` or an `action` field must never dispatch anything.
///
/// 1. `{"name": "<tool>", "arguments": {...}}` — what a model trained on the
///    function-calling wire format falls back to printing.
/// 2. `{"action": "<tool>", ...}` — the flatter form, where the remaining
///    top-level fields *are* the arguments, e.g.
///    `{"action": "web_search", "query": "terms"}`. Small local models follow
///    a single flat object far more reliably than a nested one, and it is the
///    shape the system prompt asks for by name.
///
/// The name is matched through [`resolve_tool_name`] rather than compared
/// byte-for-byte, and the arguments through [`align_arguments`] — a model weak
/// enough to print its call as text is usually also weak enough to get the
/// spelling of the name and the keys slightly wrong.
fn parse_tool_call_envelope(
    value: &serde_json::Value,
    known_tools: &[ToolDefinition],
) -> Option<(String, serde_json::Value)> {
    let named = value.get("name").and_then(|v| v.as_str());
    let arguments = value.get("arguments").filter(|v| v.is_object());
    if let (Some(name), Some(arguments)) = (named, arguments) {
        if let Some(def) = resolve_tool_name(name, known_tools) {
            return Some((def.name.clone(), align_arguments(def, arguments.clone())));
        }
    }

    let name = value.get("action").and_then(|v| v.as_str())?;
    let def = resolve_tool_name(name, known_tools)?;
    let fields = value.as_object()?;
    // `{"action": "x", "arguments": {...}}` is the two envelopes crossed, and
    // a model that emits it means the inner object — not a lone `arguments`
    // key as the argument itself.
    if fields.len() == 2 {
        if let Some(arguments) = fields.get("arguments").filter(|v| v.is_object()) {
            return Some((def.name.clone(), align_arguments(def, arguments.clone())));
        }
    }
    let arguments = fields
        .iter()
        .filter(|(key, _)| key.as_str() != "action")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some((
        def.name.clone(),
        align_arguments(def, serde_json::Value::Object(arguments)),
    ))
}

/// Shortest fragment of a name we will accept as a prefix/suffix of a
/// registered tool. Three characters (`run`, `web`, `ask`) carry too little
/// signal to be worth the risk of dispatching the wrong tool off them.
const MIN_PARTIAL_TOOL_NAME: usize = 4;

/// Folds a name to the form the match is made on: ASCII lowercase, with every
/// run of non-alphanumerics collapsed to a single `_` and camelCase humps
/// treated as the same boundary. `Web-Search`, `WEB_SEARCH`, `web search` and
/// `webSearch` all become `web_search`.
///
/// Splitting on case is a deterministic rewrite of the *same* identifier, not
/// a similarity judgement — which is the line this whole module draws.
fn normalize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev: Option<char> = None;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            let hump = ch.is_ascii_uppercase()
                && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit());
            if hump && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
        prev = Some(ch);
    }
    out.trim_matches('_').to_string()
}

/// Whether `want` is a whole leading or trailing *segment* of `name` — the
/// only partial match we accept. Anchoring on `_` boundaries is what keeps
/// `earch` from reaching `web_search` while letting `search` through.
fn is_segment_affix(name: &str, want: &str) -> bool {
    name.strip_prefix(want).is_some_and(|r| r.starts_with('_'))
        || name.strip_suffix(want).is_some_and(|r| r.ends_with('_'))
}

/// Maps the tool name a model *wrote* in a text envelope onto a registered
/// tool, in three widening passes. This decides which tool runs, so an
/// ambiguous or unrecognised name resolves to `None` — never to a best guess.
///
/// 1. Exact. The overwhelming majority, and free.
/// 2. Equal after [`normalize_ident`]. Case, hyphens and spaces are
///    presentation, not identity: no two tools can differ only in them without
///    already being indistinguishable to a user, and the ambiguity check below
///    covers the case where a registry somehow does contain both.
/// 3. A whole-segment prefix or suffix, and only when exactly one registered
///    tool matches — `search` → `web_search`. This is the case actually
///    observed in the wild (a model writing the bare verb). Two candidates —
///    `write` against both `write_file` and `write_tasks` — must fail rather
///    than pick one, because picking is how the wrong side effect happens.
///
/// Edit-distance/fuzzy matching is deliberately *not* a fourth pass. Its whole
/// premise is that a close-enough name is the right name, which is exactly the
/// judgement that must not be made here: at distance 2, `run_bash` is reachable
/// from names that have nothing to do with running a shell, and the cost of a
/// false positive (an arbitrary tool executing) is unbounded while the benefit
/// (recovering from a typo no observed model actually makes) is small.
pub(super) fn resolve_tool_name<'a>(
    written: &str,
    known: &'a [ToolDefinition],
) -> Option<&'a ToolDefinition> {
    if let Some(def) = known.iter().find(|d| d.name == written) {
        return Some(def);
    }

    let want = normalize_ident(written);
    if want.is_empty() {
        return None;
    }

    let mut normalized = known.iter().filter(|d| normalize_ident(&d.name) == want);
    if let Some(first) = normalized.next() {
        // Anything reaching here is decided by this pass alone: a second
        // candidate is an ambiguity, not an invitation to keep looking.
        return normalized.next().is_none().then_some(first);
    }

    if want.len() < MIN_PARTIAL_TOOL_NAME {
        return None;
    }
    let mut affixes = known
        .iter()
        .filter(|d| is_segment_affix(&normalize_ident(&d.name), &want));
    let first = affixes.next()?;
    affixes.next().is_none().then_some(first)
}

/// Renames arguments the model invented onto the property names the tool
/// actually declares — `max_results` onto `num_results` — using the same
/// unique-or-nothing discipline as [`resolve_tool_name`].
///
/// Unknown keys are **passed through**, never dropped. Dropping is the one
/// option that silently changes what the call means: a tool that would have
/// rejected `{"path": "/", "recursive": true}` instead runs the unqualified
/// version. A tool's own input validation is the right place to refuse a key
/// it doesn't understand, and it can only do that if it sees it.
///
/// The renaming is driven entirely by the schema each tool publishes, so it
/// stays in this layer — `smith-tools` needs no per-tool alias table, and MCP
/// tools nobody here has heard of get the same treatment.
pub(super) fn align_arguments(
    def: &ToolDefinition,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let serde_json::Value::Object(fields) = arguments else {
        return arguments;
    };
    let Some(declared) = def
        .input_schema
        .get("properties")
        .and_then(|p| p.as_object())
    else {
        return serde_json::Value::Object(fields);
    };

    // Trailing segment rather than any segment: the last word of an argument
    // name is what carries its meaning (`max_results` and `num_results` are the
    // same thing), while a shared *leading* word usually marks two genuinely
    // different arguments (`file_path` vs `file_mode`).
    let tail = |name: &str| {
        normalize_ident(name)
            .rsplit('_')
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let names: Vec<&String> = declared.keys().collect();
    let unique = |predicate: &dyn Fn(&str) -> bool| {
        let mut matched = names.iter().filter(|d| predicate(d));
        let first = matched.next()?;
        matched.next().is_none().then(|| (*first).clone())
    };

    let supplied: HashSet<&String> = fields.keys().collect();
    let mut aliased: Vec<(String, String)> = Vec::new();
    for key in fields.keys() {
        if declared.contains_key(key) {
            continue;
        }
        let normalized = normalize_ident(key);
        let want_tail = tail(key);
        let target = unique(&|d| normalize_ident(d) == normalized).or_else(|| {
            (!want_tail.is_empty())
                .then(|| unique(&|d| tail(d) == want_tail))
                .flatten()
        });
        // An alias never displaces a key the model also supplied correctly,
        // and two aliases never race for the same target — in both cases the
        // rename is ambiguous, so the key stays as written.
        if let Some(target) = target {
            if !supplied.contains(&target) && !aliased.iter().any(|(_, t)| *t == target) {
                aliased.push((key.clone(), target));
            }
        }
    }

    let mut aligned = fields;
    for (from, to) in aliased {
        if let Some(value) = aligned.remove(&from) {
            aligned.insert(to, value);
        }
    }
    serde_json::Value::Object(aligned)
}
