# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo build --workspace                    # build everything
cargo run -p smith-cli                     # run the TUI (dev)
cargo run -p smith-cli -- setup            # interactive provider/model wizard
cargo run -p smith-cli -- --provider ollama --model qwen2.5   # override provider/model for one run
cargo run -p smith-cli -- --resume <id>    # resume a saved session

cargo test --workspace                     # all tests
cargo test -p smith-tui                    # one crate
cargo test -p smith-tui app::tests::plan_gate_changed_event_syncs_state  # one test

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests live inline in `#[cfg(test)] mod tests { ... }` at the bottom of the
source file they cover — there are no separate test files. `cargo fmt`,
`cargo clippy -D warnings`, and `cargo test --workspace` must all pass before
committing.

## Architecture

`smith` is a terminal-based AI coding agent (Rust/ratatui), in the spirit of
Claude Code. It's a Cargo workspace of 7 crates with dependencies flowing one
way: `smith-core` defines traits and knows nothing about HTTP, SQLite, or
`ratatui`; every other crate implements against those traits.

- **`smith-core`** — domain types (`Message`, `ContentBlock`, `StreamEvent`),
  the `LlmProvider`/`Tool` traits, and the `Agent` orchestration loop
  (`agent.rs`). Read this crate first; `agent.rs::run_turn` is the center of
  the whole system.
- **`smith-provider`** — `LlmProvider` adapters: Anthropic, OpenAI, and
  Ollama (via the OpenAI-compatible adapter), including SSE stream parsing.
- **`smith-tools`** — built-in tools (`read_file`, `write_file`, `edit_file`,
  `list_dir`, `glob`, `run_bash`, `ask_user`) and the `ToolRegistry` that
  implements `smith_core::ToolExecutor`.
- **`smith-mcp`** — hand-rolled JSON-RPC MCP client over three transports
  (stdio, Streamable HTTP, HTTP+SSE); bridges remote MCP server tools into
  the same `Tool` trait as built-ins (default `PermissionClass::Dangerous`,
  same as `run_bash`).
- **`smith-store`** — global config (`~/.smith/config.toml`) and
  per-project session history (`.smith/sessions.db`, SQLite).
- **`smith-tui`** — the `ratatui`/`crossterm` terminal UI (chat pane, input
  box, permission/plan/question modals, sidebar). Never talks to
  `smith-provider` or `smith-tools` directly — only through the
  `Action`/`AgentEvent` channels.
- **`smith-config`** — layered configuration: global `~/.smith/config.toml`
  with `<project>/.smith/config.toml` merged over it, field by field. Split
  out of `smith-store` so reading a TOML file doesn't compile SQLite from C.
- **`smith-cli`** — binary entry point: CLI flags (`clap`), the system prompt,
  and `orchestrator.rs`, which owns the `Action` → `Agent` loop. Two frontends
  drive that same orchestrator over the same channel bundle: `smith_tui::run`
  and `headless.rs`. Headless is chosen by `-p`, by `--output-format`, or by
  stdout not being a terminal.

### The `Action` / `AgentEvent` loop

This is the pattern to understand before touching orchestration or TUI code:

1. User input in `smith-tui` (`app.rs::on_key`) produces an `Action`
   (`SubmitMessage`, `StartPlan`, `SwitchModel`, `SetPermissionPolicy`, ...).
2. `smith-tui::run` forwards it over a channel to the orchestrator
   (`smith-cli/main.rs`), which matches on it and drives `Agent::run_turn`.
3. `Agent::run_turn` streams `AgentEvent`s back (`AssistantTextDelta`,
   `ToolCallStarted`/`ToolCallResult`, `PhaseChanged`,
   `PermissionPromptNeeded`, `TokenUsage`, ...) — defined in `smith-core/src/event.rs`.
4. `App::on_agent_event` (`smith-tui/src/app.rs`) is the single place that
   turns those events into UI state (`self.lines`, `self.phase`,
   `self.activities`, modals, ...).

Adding a new capability that needs to reach the UI means: a new `AgentEvent`
variant in `event.rs`, an emitter in `agent.rs` (or `main.rs`), and a handler
arm in `app.rs::on_agent_event`.

### Tool interception pattern

`ask_user` is registered normally in `ToolRegistry` with a real
`ToolDefinition`/`permission_class`, but its `execute()` is a stub that
always errors — `Agent::run_one_tool` special-cases the tool by name *before*
the generic dispatch (permission checks, plan gate, `tools.execute()`) and
handles it separately, round-tripping through a `oneshot` channel
(`QuestionAsk`) out to the TUI and back. Use this pattern for any tool whose
result depends on interactive UI state rather than pure computation — don't
try to make `ToolExecutor::execute` itself interactive.

### Permission model

Every tool has a `PermissionClass`: `ReadOnly` (never prompts), `Mutating`
(prompts unless session-allowed), `Dangerous` (always prompts unless
session-allowed — `run_bash` and all MCP-bridged tools default here).
`PermissionPolicy` (`Ask`/`Session`/`Skip`, set via `/permission`) controls
which classes auto-allow on top of that.

Independently, `plan_gated` (set by `/plan <task>`, cleared by
`/plan approve`/`/plan reject`) blocks every tool above `ReadOnly` outright,
regardless of `PermissionPolicy` — even `Skip` doesn't bypass an unapproved
plan. Both checks happen in `Agent::run_one_tool`
(`crates/smith-core/src/agent.rs`).

### The scratch directory

Each session gets `.smith/scratch/<session_id>/` (`ToolContext::scratch_dir`)
for throwaway files — helper scripts, intermediate data. Its path is announced
in the environment block (not the static prompt: it carries the session id and
would break the prefix cache), and the system prompt tells the model project
files are for user-requested deliverables only.

The incentive is the design: a write the tool itself vouches is confined to
scratch (`Tool::scratch_scoped`, forwarded via `ToolExecutor`, checked last in
`run_one_tool`'s prompt decision) **skips the permission prompt**, so the
compliant path is also the frictionless one. Only `write_file`/`edit_file`/
`multi_edit` ever vouch, through one shared `fs_tools::scratch_confined`, which
leans on the same jail resolution as everything else — a `..` or a symlink
escaping scratch fails the prefix check and falls back to a normal prompt.
Failing closed costs one prompt, never one file. `run_bash` and MCP tools can
never vouch (they cannot bound their writes), and the default on both traits is
`false`.

Two deliberate non-exemptions: the **plan gate still blocks scratch writes**
(the exemption is about friction, not authority — an explicit user freeze
wins), and the read-before-overwrite gate applies inside scratch like anywhere
else. Stale sessions' directories are swept at startup
(`smith_tools::scratch::sweep`, 7-day TTL on the *newest* mtime anywhere in the
tree — a directory's own mtime misses rewrites of existing files), spawned off
the critical path exactly like the checkpoint sweep, and the current session is
exempt by id, never by age (`--resume` reopens old sessions legitimately).

### Turn checkpoints and `/rewind`

Before any tool above `ReadOnly` is dispatched, `Agent::run_one_tool` asks the
`ToolExecutor` which paths that call is about to write
(`Tool::snapshot_paths`, defaulting to empty) and snapshots them through the
`smith_core::checkpoint::Checkpointer` trait. One central hook rather than one
per tool: it is the same choke point the plan gate and permission prompt use,
so a refused call never leaves an object behind, and a future mutating tool is
covered the day it declares its paths.

The implementation is `smith_tools::CheckpointStore` — content-addressed blobs
in `.smith/checkpoints/objects/`, one JSON manifest per turn in
`.smith/checkpoints/turns/<session>/<seq>.json`. Deliberately no git and
deliberately not in `sessions.db`: the store has to work with no session row at
all (headless), and a manifest that lives beside the objects it indexes cannot
disagree with them.

Two rules that are load-bearing, not incidental:

- **Checkpointing never fails a turn.** Every `Checkpointer` call is
  best-effort; a failure becomes an advisory `ToolProgress` line and the tool
  runs anyway.
- **`run_bash` cannot be snapshotted.** Any Mutating/Dangerous tool that
  declares no paths — `run_bash`, every MCP tool — is recorded as *uncovered*,
  and `/rewind` says so in the report rather than implying the turn was fully
  undone.

`/rewind` restores files only; it never truncates the conversation. Instead the
orchestrator queues `Agent::note_to_model`, which rides the *next* user message
(not a message of its own — that would leave two user messages in a row).

Don't confuse `smith-tools/src/checkpoint.rs` with `staging.rs`: staging holds
the *new* bytes of a write for the moment before it is applied and deletes
itself immediately; checkpoints hold the *old* bytes and persist.

### The read-before-overwrite gate

`write_file` refuses to replace an **existing** file that the session has not
read. Creating a new file is untouched — there is nothing there to destroy.
Checkpoints can undo a blind overwrite, but the user often does not notice it
happened until much later, so this prevents rather than undoes.

The state is `smith_tools::fs_tools::ReadSet`: one `Arc`, built in
`ToolRegistry::with_builtin_tools` and shared by `read_file`, `write_file`,
`edit_file` and `multi_edit`. It cannot live on `ToolContext` (cloned per
call, so writes through a clone are lost) or in a plain `ToolRegistry` field
(the registry is behind an `Arc` and `execute` takes `&self`), so the tools —
the only per-session objects that outlive a call — hold it, with a
`std::sync::Mutex` inside. No lock is held across an `.await`; `ReadOnly`
calls really do run concurrently now, and each record is one whole critical
section.

The four judgement calls, each of which has a wrong answer:

- **Only `read_file` counts.** `grep` shows three lines out of a thousand and
  `list_dir` shows a name; treating either as knowledge would make the gate
  decorative.
- **Coverage accumulates, and partial is not whole.** Ranges are merged, so a
  long file read in chunks (what `read_file`'s own TRUNCATED note tells the
  model to do) adds up. A clipped line or a lossy decode records nothing —
  the model was shown characters the file does not contain.
- **An edit does not grant knowledge, it carries it.** Matching `old_str`
  proves the model knew that snippet, not the file, so `edit_file` never
  *creates* a reading; it refreshes an existing whole-file one across the
  change it just made. `write_file` marks what it wrote as known, because the
  model authored those bytes.
- **Knowledge is pinned to content, not to the event.** Entries are keyed on
  the sha256 of the bytes that were read (`checkpoint::hash_bytes`), so a file
  the user or `run_bash` changed afterwards is stale again. What that misses:
  a model that has *forgotten* what it read (compaction) still passes, and
  reading a file through `cat` in `run_bash` still fails — deliberately, on
  the safe side.

There is no `force` argument, and there must not be one: an escape hatch the
model can set itself is not a gate. The refusal is one step from recovery
(`read_file`, then write), which is why no user-facing override was added
either.

### Session/goal persistence

Conversations persist per-project to `.smith/sessions.db` (SQLite, via
`smith-store::SessionStore`). `/goal` is stored as a column on the session
row and folded into every request's system prompt via
`Agent::effective_system`.
Both live under the project's `.smith/` directory (gitignored), separate from
the global `~/.smith/config.toml`.

### Headless mode

`smith -p "task"` runs one turn without a terminal. `--output-format` is
`text` (prose on stdout, all chrome on stderr, so `> out.txt` gets just the
reply), `json` (one object) or `stream-json` (one `AgentEvent` per line — the
same adjacently-tagged shape `AgentEvent` serializes to).

Exit codes: `0` success, `1` the turn failed, `2` usage/config error (matching
clap's own bad-flag code), `3` stopped by a safety cap. `3` is distinct from
`1` because they warrant opposite reactions in CI — a failure is a bug, a cap
is a budget to raise with all prior work intact.

Permissions deny by default; `--allowed-tools` is the only gate, and headless
forces `PermissionPolicy::Ask` regardless of saved config (a stored `skip`
would auto-allow tools before they ever reach the channel `--allowed-tools`
inspects). `ask_user` is refused rather than answered — there is no user, and
inventing one puts words in their mouth.

### Provider/model switching mid-session

`/model` (see `smith-cli/main.rs` `Action::SwitchModel` handling) can change
just the model or rebuild the `Agent` against a different provider entirely;
conversation history carries over either way, since `messages: Vec<Message>`
lives independently of the `Arc<dyn LlmProvider>`.

### Web search backends

`web_search` (`smith-tools/src/web_search.rs`) tries five backends in order,
each falling through to the next when it is unconfigured, blocked, or errors:

1. **SearXNG** (`smith-tools/src/searxng.rs`) — the user's own instance, set
   via `[search] searxng_url`. First whenever configured, ahead of even a paid
   key: it is the only backend with no shared IP reputation and no anti-bot
   layer. Requires JSON output, which SearXNG ships **disabled** — the admin
   must add `json` under `search: formats:` in `settings.yml`.
2. **Exa** — paid, structured, reports real publication dates. Needs
   `[exa] api_key`. Skipped entirely without one; the keyless tier now answers
   HTTP 402, so probing it only cost a request per search.
3. **Bing over RSS**, plain HTTP (`smith-tools/src/bing.rs`) — the free
   workhorse, and what makes `web_search` work with no configuration at all.
   `&format=rss` returns the same ten results as ~5 KB of stable XML rather
   than ~122 KB of HTML, with real target URLs instead of Bing's `ck/a`
   redirect wrapper.
4. **Bing over RSS, through headless Chromium** (`smith-tools/src/chromium.rs`)
   — the same feed on a different network path, for hosts where plain HTTP is
   intercepted or fingerprinted. Chromium's XML viewer leaves the feed's markup
   intact in the dumped DOM, so tiers 3 and 4 share one parser.
5. **DuckDuckGo lite over plain HTTP** — last, and measured as blocked far more
   often than not.

Two things here are counterintuitive and load-bearing:

- **The Bing `setmkt`/`setlang` parameters are not cosmetic.** Without them
  Bing answers 200 with ten well-formed results that have nothing to do with
  the query — structurally indistinguishable from success. `bing::looks_poisoned`
  is the guard: the measured signature is *no* result matching *any* query term.
  A poisoned response is retried under the machine's locale market before the
  tier is written off.
- **Bing's RSS `<pubDate>` is a crawl timestamp, not a publication date** (every
  item carries today's date), so it is deliberately dropped. SearXNG and Exa are
  the only tiers that contribute a recency signal.

DuckDuckGo used to be tiers 2 and 3. It was demoted on evidence: its `html` and
`lite` endpoints answer HTTP 202 with a 14 KB challenge page to a plain client
*and* to a real headless browser, and its JavaScript endpoint renders no results
at all under `--dump-dom` at any virtual time budget. `chromium.rs` is
correspondingly now a generic page fetcher that knows nothing about result
markup — the caller picks the URL and owns the parsing.

Results are cached per session on the normalised query, so two near-identical
searches in one turn cost one request. Failures distinguish three cases, and
collapsing any of them is what previously ended with the model quietly answering
from training data: "found nothing" (a backend ran), "TEMPORARILY BLOCKED —
retry shortly" (`Unavailable::Transient`), and "UNAVAILABLE — nothing is set up"
(`NotConfigured`/`Misconfigured`). None of them ever licenses answering from
memory.

Only the pure halves are unit-tested; two `#[ignore]`d tests
(`web_search::tests::live_search_returns_results_relevant_to_the_query` and
`chromium::tests::live_fetch_returns_a_parseable_search_feed`) exercise the real
network and browser.

### MCP servers

`[[mcp_servers]]` entries carry either a `command` (stdio) or a `url`
(`transport = "http" | "sse"`, or unset to try Streamable HTTP and then
HTTP+SSE). `command`-only entries written before URL support keep working
untouched — the presence of `url` is what selects a network transport.
`smith_mcp::transport::Transport` is the whole abstraction: `send` one JSON-RPC
message, receive on an `Incoming` channel. `McpClient` is written against that
pair alone, so all three transports share one correlator, one 30s request
deadline and one liveness rule (**the incoming channel closing** fails every
in-flight call at once, so a server dying mid-session costs no timeouts).

`McpRegistry::connect_all` connects every server **concurrently**, each capped
at `CONNECT_TIMEOUT` (15s), and `run_orchestrator` starts it before anything
else and joins it immediately before `Agent::new` — so N servers cost the
slowest one, overlapped with provider/memory/subagent setup. The UI never
waited: `main` spawns the orchestrator and renders in parallel.

Three decisions worth not re-litigating:

- **Resources are a tool, not context.** `list_mcp_resources` /
  `read_mcp_resource` are registered only when some server actually publishes
  resources. A resource list injected into every request would cost tokens on
  every turn of the session, forever, and go stale; a tool is paid for only
  when used. `read_mcp_resource` is `Mutating` for `web_fetch`'s reason (a
  model-composed URI leaving the machine); the listing is `ReadOnly`.
- **Prompts are user-invoked, never model-reachable.** `/mcp prompt [<server>]
  <name> [key=value ...]` → `McpRegistry::render_prompt` → the text of one user
  message. No tool exposes them: a prompt template is *meant* to be an
  instruction, and the only thing that makes that safe is that the user asked
  for it by name.
- **Everything else a server says is data.** Tool results and resource
  contents go through `untrusted::fence` — `web_fetch`'s framing verbatim,
  stated before *and* after the body, with every run of five hyphens defanged
  so a payload cannot close the fence. Tool *descriptions* cannot be fenced
  (an ignored description is an unusable tool), so they get provenance and a
  4 KB cap; what actually protects those is the `mcp__{server}__{tool}`
  namespacing and `Dangerous`.

### The JSON action envelope

Models with no usable structured tool channel (small local ones especially)
can call a tool by replying with nothing but
`{"action": "web_search", "query": "..."}`. `Agent::recover_text_tool_call`
(`smith-core/src/agent.rs`) rebuilds any such object into a real `ToolUse`
before the turn ends, so it flows through the ordinary permission/execution
path. Two envelopes are accepted — that flat one, where the remaining
top-level fields *are* the arguments, and the nested
`{"name": ..., "arguments": {...}}` form. Both only fire when the name
resolves to exactly one registered tool: an `action` field is common enough in
ordinary JSON that dispatching on it blindly would turn quoted data into tool
calls.

Resolution (`resolve_tool_name`) is tolerant but never speculative. It accepts
an exact name, a name equal after normalisation (`Web-Search`, `WEB_SEARCH`,
`webSearch` → `web_search`), or a whole-segment prefix/suffix of at least four
characters (`search` → `web_search`) — and only when **one** registered tool
matches. `write` against both `write_file` and `write_tasks` resolves to
nothing rather than to a guess, and edit-distance matching is deliberately
absent. Arguments get the same treatment from `align_arguments`, which renames
an invented key onto the schema's own property when that is unambiguous
(`max_results` → `num_results`); keys it can't place are passed through for the
tool to reject, never dropped here.

### Reasoning tags in the text channel

Reasoning models emit `<think>`/`<thinking>`/`<reasoning>` blocks in the
*text* channel whenever the provider has no separate reasoning stream.
`ReasoningFilter` (`smith-core/src/agent.rs`) strips them inside
`consume_stream` — one shared place, because the leak follows the model, not
the wire format — before the deltas are forwarded *or* accumulated, so nothing
reaches the transcript, history, or the next request. It works on streamed
deltas (a tag may straddle two), leaves anything inside a ``` fence or
immediately after a backtick alone, and removes a stray closing tag without
eating the text around it. The reasoning itself is discarded; `Agent::
reasoning_tags_stripped()` counts what was removed.

### Subagents (the `task` tool)

`task` delegates a bounded piece of work to a **child `Agent`** with its own
history. The child runs a full `run_turn`, and the only thing that crosses back
into the parent's history is its final message — everything it read is
discarded. That asymmetry is the whole feature: measured inline in
`agent.rs::tests`, six 4 KB reads leave the parent 8074 estimated tokens when
it does them itself and 58 when it delegates.

The mechanism is **interception**, like `ask_user` and `write_tasks`, but for a
structural reason rather than a UI one: a subagent has to construct an `Agent`
from the parent's provider, tool registry, model and context, and an ordinary
`Tool` (`&self`, in `smith-tools`) can reach none of those.
`Agent::run_task` (`smith-core/src/agent.rs`) builds the child; everything else
lives in `smith-core/src/subagent.rs`. `run_task` returns a `BoxFuture` rather
than being an `async fn` — `run_turn` → `run_one_tool` → `run_task` →
`run_turn` is a real recursion, and naming the type is what breaks the `Send`
inference cycle.

The five limits, all of them load-bearing:

- **Read-only, always.** A child's tool set is the registered `ReadOnly` tools
  minus `ask_user`/`write_tasks`/`task`, and a definition asking for
  `write_file` is refused that tool (visibly, as a progress line) rather than
  granted it. Reads are what fill a context window; a write is one `tool_use`
  the parent can emit itself after reading the report. Enforced by
  `subagent::RestrictedTools`, not by the prompt.
- **Depth 1.** A child never sees `task`, *and* `run_task` refuses on depth —
  the JSON-envelope fallback resolves names against the registry, not the
  visible set.
- **A shared tool-call pool per turn.** `Agent::subagent_tool_budget` is
  refilled from `max_tool_calls_per_turn` at the top of every `run_turn` and
  drained by every child. Per-child caps multiply (50 parent calls × 30 each);
  a pool is additive, so a turn spends at most twice its own cap. A child also
  gets at most the parent turn's *remaining* wall clock.
- **Permission prompts are refused, not routed up.** The modal is keyed by
  `tool_call_id`, and a child's ids belong to a transcript the user cannot see,
  so routing it up would ask them to approve a call with no card. Session
  grants are not inherited either, and neither is a `skip` policy.
- **`ask_user` is refused with a reason**, through the `Err(reason)` arm of
  `QuestionAsk` that headless already uses.

Cancellation: the child holds `cancel.child_token()`, so Esc reaches it
immediately, and `run_task` still returns a real `ToolResult` — the parent's
`tool_use` gets its `tool_result` exactly as for any other tool.

Visibility is `AgentEvent::ToolProgress` on the parent's `task` card, emitted
by `subagent::relay_child`, which drains the child's private event channel and
turns each step into one line ("`general-purpose: [3] Read file src/agent.rs`").
No new event variant, so `stream-json` and the TUI already render it. The one
thing forwarded verbatim is `TokenUsage` — the user pays for those tokens —
and the child's total is also billed to the parent's turn.

A child that runs out of budget still returns whatever it wrote, with a
`[This report is partial — …]` note (`finish_subagent`). Throwing partial work
away makes the parent re-delegate and pay twice for the half it already had.

Definitions live in `~/.smith/agents/*.md` with `name` / `description` /
`tools` / `model` front matter and a markdown body appended to the child's
prompt. `SubagentDefinition::parse` is in `smith-core`; the directory scan is
`smith-cli/src/subagents.rs` (only the frontend knows about home directories).
A broken file costs its own subagent and is reported — never startup. The
built-in `general-purpose` child needs no file and cannot be shadowed by one.

### User extension files: commands, skills, personas

Three features, one shape: markdown on disk, discovered in a project directory
and a global one, folded into a turn. The shared half — the two roots, the
recursive walk, the size/count/depth caps, the optional `---` front matter
parser, and the path jail — is `smith-config/src/extend/mod.rs`; the three
submodules hold what differs. The jail delegates to `memory::real_path`, the
same function `SMITH.md`'s `@import` uses, because a path jail that exists
twice is a path jail that will be fixed once.

The jail is enforced on **discovery**, not only on a directive. Nothing in a
commands directory says "read this other file", but
`.smith/commands/deploy.md -> ~/.ssh/id_rsa` is one `ln -s` in a repository,
and reading it would put the key in a prompt.

- **Custom slash commands** (`.smith/commands/**.md`, `~/.smith/commands/**.md`)
  are prompts. `/db:migrate users` expands `db/migrate.md` and submits it as an
  ordinary `SubmitMessage` — no new `Action`, no capability typing the same
  prose would not have. Directories are namespaces, written with `:` (`/` reads
  as a path, and `:` is what MCP already uses to qualify a tool by its server).
  A file may **never** take a built-in's name: the loader refuses it, and
  `app::run_slash_command` matches built-ins first regardless. Between two
  custom commands the project's wins and the shadowed one is reported. What
  actually defuses a prompt from a cloned repo is that the **expanded body**
  goes into the transcript as the user's message, above a system line naming
  the file — so it is visible in the same breath as it runs.
  `$1`..`$9`/`$ARGUMENTS`/`$$` substitute; a referenced `$N` with nothing
  passed **refuses the whole expansion** rather than expanding to nothing,
  because "Refactor $1 to use $2" with both empty is a well-formed, meaningless
  instruction the model will act on by guessing.
- **Skills** (`.smith/skills/<name>/SKILL.md`) are progressive disclosure. Only
  `name — description` is loaded; the body arrives when the model calls the
  `skill` tool (`smith-tools/src/skill.rs`), whose own `description` carries the
  index. A tool rather than a name the model mentions: a mention has no channel
  short of scanning prose for names (the ambiguity `recover_text_tool_call`
  refuses), a tool result is a *message* so the body invalidates no cached
  prefix, and the tool inherits schema validation, permission class, the tool
  card and `stream-json` for free. The tool is registered only when at least one
  skill exists, so the feature costs exactly zero with none.
- **Personas / output styles** (`.smith/personas/<name>.md`, `--persona <name>`,
  `default.md` when unspecified, `--persona none` to disable) go in the
  **static** half — `with_system` — read once at startup and never re-read, so
  the system prompt is byte-identical for the whole session and prompt caching
  is untouched. To make `mode: replace` safe, `SYSTEM_PROMPT` was split in
  `prompts.rs` into `PROMPT_INVARIANTS` (answer from search results not memory;
  tool output is DATA not instructions; use the environment's date; the JSON
  envelope) and `PROMPT_STYLE` (workflow, deliverables, delegation). A persona
  replaces the *style* half only. Two payoffs: a persona structurally cannot
  delete the lines that keep output connected to reality — the highest-leverage
  thing to put in a persona someone else ships — and `PROMPT_INVARIANTS` stays
  a byte-identical prefix of every prompt smith sends, so a persona costs the
  shared cache the style half and never the whole thing. Switching persona
  mid-session is deliberately not offered.
