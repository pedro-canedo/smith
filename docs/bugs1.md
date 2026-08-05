Prompt pronto para colar no agente:

# Bug: Ollama 400 — `invalid message content type: <nil>` after tool use
## Symptom
With Ollama (e.g. `qwen3.5:9b`) via the OpenAI-compatible adapter, simple chat works.
After a complex request that triggers tools (e.g. “write a small index.html”), the next
turn fails with:
provider returned an error: 400 Bad Request: {"error":{"message":"invalid message content type: ", "type":"invalid_request_error","param":null,"code":null}}

It looks like the model “dies” on complex tasks; the real failure is on the *follow-up*
request that re-sends history containing a bad assistant message.
## Root cause
In `crates/smith-providers/src/openai.rs`, `messages_to_wire` serializes
tool-only assistant messages as:
```json
{ "role": "assistant", "content": null, "tool_calls": [...] }
OpenAI accepts content: null with tool_calls. Ollama’s OpenAI-compatible API requires content to be a string and rejects null with exactly that error. Same class of bug has bitten pydantic-ai, ruby_llm, etc.

Relevant code (~lines 266–268):

"content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text) },
Fix (required)
Use empty string "" instead of Null when assistant text is empty (especially when tool_calls is present). Safe for both OpenAI and Ollama.
Do not emit an assistant wire message that is empty and has no tool_calls (skip it, or give it content: "" only if something else requires the slot).
Add a unit test in openai.rs that asserts a tool-only assistant message serializes to "content": "" (not JSON null), with tool_calls present.
Keep English-only user-facing strings / comments (project OSS default).
Hardening — avoid the same class of bugs elsewhere
While fixing, audit the OpenAI/Ollama wire path for other provider incompatibilities that break after tool use or multi-turn:

Assistant content: never send null/missing typed content Ollama can’t parse; prefer "".
Tool-call IDs: some Ollama streams omit or delay tool_calls[].id on early chunks — ensure we don’t drop argument deltas or invent broken history when id is missing (generate a stable fallback id if needed).
Tool message shape: role: "tool" messages must always have string content and a valid tool_call_id matching the prior assistant tool_calls.
Empty / cancelled turns: don’t persist or re-send assistant messages with empty content and no tool calls that would poison the next request.
stream_options.include_usage: confirm older Ollama versions don’t reject the body; if they do, gate or omit for Ollama-only.
Anthropic path: leave alone unless the same null-content pattern exists there; this bug is specifically in the OpenAI-compatible serializer used by Ollama.
Acceptance
Reproduce: Ollama + tools → ask to create a file → approve tool → send a follow-up message → no 400; conversation continues.
cargo test -p smith-providers covers the empty-content serialization case.
cargo fmt, clippy -D warnings, cargo test --workspace pass.