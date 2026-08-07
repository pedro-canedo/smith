//! Reading one field out of a tool call's arguments.
//!
//! Three accessors rather than a `#[derive(Deserialize)]` struct per tool, and
//! that is deliberate. `ToolRegistry::execute` already validates every call
//! against the schema the model was shown, so typed deserialization would not
//! be adding a check — it would be adding a second, differently-worded one.
//! And `schema_validate` does not reject unknown keys on purpose, because
//! `smith_core::agent::align_arguments` renames an invented argument name onto
//! a declared one and passes through what it cannot place; a
//! `deny_unknown_fields` anywhere in that path would undo the recovery in
//! silence.
//!
//! What these do fix is the real duplication: the same
//! `input.get(k).and_then(|v| v.as_str())` was written out at fifteen call
//! sites across five modules, and `fs_tools` had two private helpers for it
//! that nobody else could reach.
//!
//! These are for *arguments*. Parsing a provider's response — Exa's or
//! Tavily's JSON, say — is a different job with different failure modes, and
//! `web_search` keeps doing that by hand.

pub(crate) fn field_str<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

pub(crate) fn field_bool(input: &serde_json::Value, key: &str) -> Option<bool> {
    input.get(key).and_then(|v| v.as_bool())
}

pub(crate) fn field_u64(input: &serde_json::Value, key: &str) -> Option<u64> {
    input.get(key).and_then(|v| v.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_key_and_a_wrongly_typed_one_read_the_same() {
        let input = serde_json::json!({"path": 7, "ok": "yes"});
        assert_eq!(field_str(&input, "path"), None);
        assert_eq!(field_str(&input, "absent"), None);
        assert_eq!(field_bool(&input, "ok"), None);
        assert_eq!(field_u64(&input, "path"), Some(7));
    }

    #[test]
    fn a_present_field_of_the_right_type_reads_back() {
        let input = serde_json::json!({"path": "src/main.rs", "hidden": true, "limit": 20});
        assert_eq!(field_str(&input, "path"), Some("src/main.rs"));
        assert_eq!(field_bool(&input, "hidden"), Some(true));
        assert_eq!(field_u64(&input, "limit"), Some(20));
    }
}
