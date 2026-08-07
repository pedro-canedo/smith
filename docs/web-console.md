# RFC: the Smith Web Console

A local web frontend for a running smith session: the live transcript, the
approvals the turn is blocked on, a Kanban board of the agent's tasks, and the
project's session history — served from the smith process itself, on
loopback, behind a per-run token.

The one sentence that constrains everything else: **the console is a second
frontend, not a second agent.** `smith-tui` and `headless` already drive the
same orchestrator over the same four channels; the console is a third consumer
of exactly that contract. It reimplements no permission logic, no plan gate,
no checkpointing, no pricing — it renders events and submits actions.

Status: P0 as designed here. Sections marked *(P1)*/*(P2)* are direction, not
commitment.

---

## 1. Architecture

```
                        ┌────────────────── smith-cli (composition root) ─────────────────┐
                        │                                                                 │
 orchestrator ──mpsc──▶ │ pump ──mpsc──▶ smith_tui::run             (TUI unchanged)       │
   (event_tx)           │  │                                                              │
                        │  ├──▶ SessionProjection (Arc<RwLock>)   ← late-joiner snapshot  │
                        │  └──▶ broadcast(1024) with seq numbers ──▶ SSE handlers (N)     │
                        │                                                                 │
 permission_rx ┐        │                                                                 │
 question_rx   ┴──────▶ │ ask broker (owns the oneshots; first answer wins)               │
                        │   ▲ mpsc<AskAnswer> ◀── TUI modal keys                          │
                        │   ▲               ◀── POST /api/ask/answer                      │
                        │                                                                 │
 action_tx (clonable) ◀─┤ POST /api/action (ActionDto whitelist)                          │
                        └─────────────────────────────────────────────────────────────────┘
```

Three pieces, all in the composition root:

- **The pump** sits between the orchestrator's event channel and the TUI,
  *only when the console is enabled*. It stamps each event with a sequence
  number, applies it to a `SessionProjection` (the state a late-joining
  browser needs), forwards it to the TUI's own unbounded mpsc, and publishes
  it on a bounded `tokio::sync::broadcast`. With the console off, `event_rx`
  goes straight to `smith_tui::run` and the path is byte-identical to today.
  Headless never gets a pump, a broker, or a server.

- **The ask broker** owns `permission_rx`/`question_rx` — the channels whose
  messages carry a `oneshot::Sender` that can be consumed exactly once. Both
  frontends learn of a pending ask from the `AgentEvent` the agent already
  emits (`permission_prompt_needed` / `user_question_needed`); both submit
  answers on a shared `mpsc<AskAnswer>`; the broker resolves the oneshot on
  the **first** answer and drops the rest. The broker runs whenever the TUI
  does — console on or off — so there is exactly one ask-resolution mechanism
  in interactive mode. Headless keeps its own (`--allowed-tools`), untouched.

- **The server** is a hand-rolled HTTP listener in
  `smith-cli/src/webconsole/`, sharing the security predicates of the
  existing `smith setup web` server (extracted to `webguard/`). REST for
  commands and reads, SSE for the event stream.

### Why not a websocket, and why not axum

The outbound stream is already a public wire format: `AgentEvent` serializes
adjacently tagged (`{"type": …, "data": …}`) and that *is*
`--output-format stream-json`. An SSE `data:` line is that line, verbatim —
the console's stream and a `smith -p … --output-format stream-json` pipe are
the same bytes. Inbound traffic is a handful of small commands, which are
POSTs. A websocket would add a framing layer and a dependency to gain
bidirectionality nothing needs.

axum was considered and rejected for the same reason the config server is
hand-rolled (`webconfig/mod.rs` states it): a framework's default is "route it
and let the handler decide"; ours is "refuse unless every predicate holds."
The audited `Guard` — route whitelist, exact `Host` match, `Origin`,
`Sec-Fetch-Site`, constant-time token compare, body caps — carries over
unchanged. Reimplementing it as tower middleware would be a second security
mechanism, which is the thing this repo does not do.

---

## 2. Wire protocol

### 2.1 Outbound: SSE at `GET /api/events`

- Each event is one frame: `id: <seq>` then `data: <stream-json line>`.
  `seq` increments per event for the life of the process.
- On connect the server sends a synthetic `hello` frame
  (`{"type":"hello","data":{"seq":<current>}}`) so the client knows where the
  stream starts.
- A `: ping` comment frame every 15 s keeps intermediaries from reaping the
  connection.
- If a subscriber lags past the broadcast buffer (1024 events), it receives a
  synthetic `{"type":"gap"}` frame. **Drop is visible, never silent**: on
  `gap` — and on every EventSource `open`, including auto-reconnect — the
  client refetches `GET /api/state` and resumes from its `seq`, discarding
  SSE frames with `id <= state.seq`.
- The two synthetic types (`hello`, `gap`) exist only on this endpoint; they
  are not `AgentEvent`s and never appear in `--output-format stream-json`.

### 2.2 Snapshot: `GET /api/state`

A `SessionProjection` — the accumulated state a browser opening mid-turn
needs to render before the stream continues:

```jsonc
{
  "seq": 412,
  "session_id": "…", "provider": "…", "model": "…",
  "phase": "working", "plan_gated": false,
  "transcript": [ /* closed items: user/assistant/system/tool cards */ ],
  "streaming_text": "…",                  // open assistant delta accumulation
  "tasks": [ /* current stamped snapshot */ ],
  "pending_permission": { "tool_call_id": "…", "tool_name": "…", "detail": "…" } | null,
  "pending_question":   { "id": "…", "prompt": "…", "options": ["…","…","…"] } | null,
  "usage": { "input_tokens": …, "output_tokens": …, "cache_read": …, "cache_write": … },
  "cost_usd": 0.0142, "unpriced_turns": 0,
  "context": [79000, 128000, true] | null  // used, window, estimated
}
```

The projection is maintained by the pump — it is `App::on_agent_event`
without a screen. Cost and usage are *read from events*, never recomputed:
the frontend has no pricing table, on purpose (the same rule
`SessionStore`/`session_cost` already enforce).

### 2.3 Inbound: `POST /api/action`

Body is an `ActionDto` — a deliberate whitelist, **not** a serde derive on
`Action`. `Action` stays serde-free because a blanket `Deserialize` would put
`Quit` (kills the session), `SetPermissionPolicy` (changes authority), and
`Mcp` one JSON body away from any token holder. The DTO is the Guard
philosophy applied to the payload:

```jsonc
{"type":"submit_message","data":"…"}   // when phase == idle
{"type":"interject","data":"…"}        // mid-turn
{"type":"cancel_generation"}
```

Deliberately absent from P0: `Quit`, permission policy, model switching, MCP,
rewind, plan actions. Each needs its own argument before joining.

### 2.4 Ask answers: `POST /api/ask/answer`

```jsonc
{"kind":"permission","tool_call_id":"…","decision":"allow_once"|"allow_session"|"deny"}
{"kind":"question","id":"…","answer":"…"}
```

`404` when there is no such pending ask (already answered from the other
frontend, or the turn was cancelled). First answer wins; the loser's modal is
dismissed by the resolution events below.

### 2.5 New `AgentEvent` variants

Two additive variants (changelog note; stream-json consumers with exhaustive
`type` switches should treat unknown types as skippable):

```
permission_resolved { tool_call_id, decision, source: "tui"|"web" }
question_resolved   { id, source }
```

Emitted by the broker when it resolves an ask. The TUI closes a stale modal
on a matching id and, when `source == "web"`, appends a system line so the
terminal user sees the approval happened elsewhere. Headless never runs the
broker, so these never appear in P0 stream-json output — but they are part of
the contract from now on.

---

## 3. The Kanban board

The board is an evolution of `write_tasks`, not a parallel mechanism. One
tool, one event, one snapshot — the columns are the statuses.

### 3.1 Schema

`Task` gains three optional fields; `TaskStatus` gains two variants:

| Field | Type | Written by | Semantics |
|---|---|---|---|
| `content` | `String` | model | unchanged |
| `status` | `pending \| in_progress \| blocked \| review \| completed` | model | `blocked` = cannot proceed (reason required in spirit); `review` = done pending the user's judgement |
| `id` | `Option<String>` | model, else stamped | stable identity across full-list replacements; `run_write_tasks` stamps `"t{n}"` positionally when absent; the tool schema tells the model to echo ids back |
| `blocked_reason` | `Option<String>` | model | one line; meaningful with `blocked` |
| `updated_at` | `Option<u64>` (ms epoch) | **smith, at receipt** | stamped by `run_write_tasks`; a model-supplied value is discarded; refreshed only when `(content, status, blocked_reason)` changed vs. the previous snapshot's task with the same id |

All additions are `#[serde(default)]` + `skip_serializing_if` — a task
without them serializes byte-identically to today, so the wire change is
additive. The one behavioural compat note: the **new status strings reach
stream-json** whenever the model uses them; consumers that match `status`
exhaustively must add arms.

### 3.2 Persistence

Migration v4 in `smith-store`: `tasks(session_id TEXT PRIMARY KEY,
snapshot TEXT NOT NULL, updated_at INTEGER NOT NULL)` — one row per session,
the whole stamped snapshot as JSON. Saved beside `persist_turn`. Resume
prefers the snapshot (the stamps exist only there — history holds the model's
un-stamped `tool_use` input) and falls back to the legacy history scan for
sessions without a row.

### 3.3 Prompt and skills

`PROMPT_STYLE`'s task-tracking bullet grows: mark a step `blocked` with a
one-line `blocked_reason` and move on rather than stalling; use `review` for
work that awaits the user's judgement. The builtin skills that drive task
usage (`goal`, `loop`, `fix-bug`, `new-feature`) get matching lines.

---

## 4. Security model

Inherited from the config server (`webguard/`), predicate for predicate:

- **Loopback only.** `127.0.0.1`, ephemeral port unless `[web] port` pins
  one. Binding beyond loopback is not offered in P0; the RFC for that day
  starts with real authentication, not a flag.
- **Per-run token.** 244 bits (two UUIDv4s, base64url), minted at startup,
  shown only in the URL the TUI displays. Never logged, never written to
  disk.
- **Guard predicates on every request**, in order: route whitelist → no
  `Transfer-Encoding` → exact `Host` match (DNS rebinding) → `Origin` match
  if present → `Sec-Fetch-Site` ∈ {same-origin, none} → token
  (constant-time) → write routes require `application/json` and a capped
  `Content-Length`. Refusals are generic (403/404/400) — the reason is never
  sent.
- **Token placement**: `X-Smith-Token` header everywhere, with two
  exceptions that take `?t=` — the page URL (a link must be clickable) and
  `GET /api/events` (the EventSource API cannot set headers). Both are the
  same secrecy class as the link itself.
- **CSP** on the shell: `default-src 'none'; script-src 'unsafe-inline';
  style-src 'unsafe-inline'; connect-src 'self'; img-src data:;
  form-action 'none'; frame-ancestors 'none'; base-uri 'none'` — the
  single-file app needs inline script/style and same-origin fetch, nothing
  else.
- **No secrets cross the wire.** P0 has no settings surface; nothing reads
  or writes `config.toml`. When settings arrive *(P1)*, the config server's
  rule applies: the browser learns whether a key is set, never the key.

### 4.1 The approvals surface is the sharp edge

The permission answer is the only rung in smith's authorization ladder that
*adds* authority (`docs/authorization.md`). Two hazards the console UI must
surface rather than smooth over:

- **A session grant is per tool name, not per call.** "Allow for session" on
  a `run_bash` that shows `ls` authorizes every future shell command this
  session. The web modal carries that sentence next to the button, verbatim.
- **The detail line is a summary, not the action.** `PermissionRequest.detail`
  is one line built server-side; the console must not imply it shows
  everything the call will do.

### 4.2 The anti-daemon question

`webconfig/mod.rs` states "there is no `smith serve`, no daemon, and no
starting it alongside a chat session" — for the **credential endpoint**, a
socket that can rewrite `~/.smith/config.toml`. The console is a different
endpoint with a different threat model: it reads and writes no secrets, and
what it *can* do — drive the agent — is exactly what the terminal in front of
the user already does, behind the same permission ladder. What carries over
is everything that made the config server safe (loopback, token, Guard); what
changes is the lifetime: the console lives exactly as long as the interactive
session that started it, dies with it, and cannot outlive or restart it.
There is still no daemon.

---

## 5. Lifecycle

- Opt-in: `--web` flag or `[web] enabled = true` (layered config, flag ORs).
  `--web-port` / `[web] port` pin a port; default is ephemeral.
  `[web] open_browser` reuses the config server's WSL-first launcher.
- Startup order in the TUI path: channels → broker → pump → orchestrator →
  bind → mint token → serve → `TuiConfig.console_url`. The URL shows on the
  idle splash and in the sidebar's Session tab.
- `GET /` 302s to `/s/<session_id>`; a wrong session id in the path is a 404
  (never a redirect that would confirm the real id).
- Quit in the TUI aborts server, pump, and broker exactly where the
  orchestrator is aborted today. EventSource reconnect then fails and the app
  shows "session ended". There is no web-initiated shutdown: the console
  cannot kill the terminal session.
- Headless (`-p`, `--output-format …`) never starts any of this; CI behavior
  is byte-identical.

Known limits, accepted: HTTP/1.1 allows ~6 connections per origin, so ~6
concurrent tabs starve (one SSE each) — loopback tooling does not justify
HTTP/2. A slow tab sheds events via the visible `gap` path, never by
blocking the TUI (whose mpsc is unbounded and fed first).

---

## 6. Frontend

`web/` at the repo root: Vite + React + TypeScript + Tailwind + shadcn/ui,
built single-file with `vite-plugin-singlefile`. The committed artifact is
exactly one file — `web/dist/index.html` — embedded with `include_str!`, the
same decision `webconfig/ui.html` already argued (no asset resolver on a
privileged socket; zero static routes in the whitelist).

- **Identity**: the Ember palette from `docs/design-system.md`, hand-copied
  into CSS variables with the doc named as source of truth. A Rust test pins
  the committed HTML against `Theme::token_hex` values — the same mechanism
  that pins `ui.html` — doubling as a coarse staleness tripwire.
- **Surfaces (P0)**: Session Live (transcript, tool cards, streaming text,
  composer that submits when idle and interjects mid-turn, permission and
  question modals), Approvals (the pending ask, prominent), Kanban (five
  status columns, `blocked_reason` on the card, `updated_at` recency),
  History (session list from `sessions.db`, read-only transcript).
- **Dev loop**: `pnpm -C web dev` with a Vite proxy for `/api` to a running
  `smith --web` port; the token comes from the printed URL via
  `VITE_SMITH_TOKEN`. Node is not part of the Rust gates;
  `scripts/build-web.sh` rebuilds and the result is committed.

---

## 7. P0 API table

| Method | Path | Auth | Purpose | Errors |
|---|---|---|---|---|
| GET | `/` | `?t=` | 302 → `/s/<id>` | 403 |
| GET | `/s/<id>` | `?t=` | app shell | 403, 404 |
| GET | `/api/state` | header | `SessionProjection` snapshot | 403 |
| GET | `/api/events` | `?t=` | SSE stream | 403 |
| POST | `/api/action` | header | `ActionDto` → orchestrator | 400, 403 |
| POST | `/api/ask/answer` | header | resolve pending ask | 400, 403, 404 |
| GET | `/api/sessions` | header | session list | 403, 500 |
| GET | `/api/sessions/<id>/messages` | header | persisted history | 403, 404, 500 |
| GET | `/api/tasks` | header | current board snapshot | 403 |

Admitted-but-failed requests answer `{"error":"…"}`; Guard refusals stay
bare. History reads use the console's own read-only `SessionStore` handle.

---

## 8. Later phases

*(P1)* **Fleet**: `channels()` already supports N bundles; fleet is N
orchestrators in one process plus a session registry keyed by id, `/s/<id>`
routing across them, and a global approvals inbox. **Settings**: the config
server's read/write surface, merged behind the same Guard, same
secrets-one-way rule. **Scheduler**: persisted prompts on a timer reusing
`/loop` semantics and headless's unattended rules (`--allowed-tools`,
`PermissionPolicy::Ask` forced). **Analytics**: the `turns` table already
holds per-turn tokens/cost.

*(P2)* Worktree-isolated parallel agents; remote access (which begins with
real authentication, not a bind flag); push notifications.

---

## 9. Alternatives considered

- **axum + WebSocket** — rejected; §1. The framework buys routing we have and
  costs a reimplementation of the Guard as middleware.
- **A ninth crate (`smith-web`)** — rejected. The server is composition-root
  code: it needs the Guard, the token minter, the channel handles, and the
  session identity, all of which live in or are wired by `smith-cli`. The
  compiler boundary a crate buys is decorative here; the file-size gate keeps
  the module tree honest instead.
- **A new `update_kanban` tool** — rejected. Two tools for one concept makes
  the model choose between them; every builtin skill already names
  `write_tasks`. Additive evolution keeps one mechanism.
- **`phase_changed` as modal dismissal** — rejected. The phase after an
  approval depends on what runs next; inferring "your modal is stale" from it
  is the fragility the explicit `*_resolved` events remove.
- **broadcast-always (pump unconditionally)** — rejected. With the console
  off, the TUI path should be byte-identical to today, not "identical modulo
  one forwarding task".
