# Authorization in smith

Derived from `Agent::run_one_tool` and `Agent::dispatch_tool`
(`crates/smith-core/src/agent.rs`), not from prose. Where the code and the
prose disagree, the code is what this document records — and the disagreements
are listed at the end, because they are the useful part.

There are six mechanisms that can stop a tool call, plus two more that
constrain what a call may do once it is allowed to run. They are not
alternatives to each other: a call has to survive **all** of them, in a fixed
order, and none of them can grant what an earlier one withheld.

---

## The order

For one `tool_use` block named `T` with arguments `A`:

| # | Gate | Where | Can it *deny*? | Can it *grant*? |
|---|------|-------|----------------|-----------------|
| 0 | Tool visibility (`tool_defs`, `RestrictedTools`) | provider request / `subagent.rs` | yes | no |
| 1 | Name interception (`ask_user`, `write_tasks`, `task`) | `run_one_tool` | n/a | **skips 2–5** |
| 2 | `plan_gated` | `run_one_tool` | yes | **no** |
| 3 | `PreToolUse` hook | `run_one_tool` (see [hooks.md](hooks.md)) | yes | **no** |
| 4 | `PermissionClass` × `PermissionPolicy` × `allowed_session_tools` × `scratch_scoped` | `run_one_tool` | it decides *whether to ask* | no |
| 5 | The permission answer (modal, or `--allowed-tools` in headless) | TUI / `headless.rs` | yes | yes, for this call and optionally the session |
| 6 | `schema_validate` | `ToolRegistry::execute` | yes | no |
| 7 | The tool's own checks (`ReadSet`, path jail, argument sanity) | each tool | yes | no |

Rung 0 is not really a gate — it is what the model can *see*. A tool that is
not in `tool_defs` can still be named (the JSON action envelope resolves names
against the registry, and a model can hallucinate one), so nothing downstream
may assume rung 0 held.

### 1. Name interception — the only thing that skips rungs

```rust
if name == "ask_user"    { return self.run_ask_user(..)   }
if name == "write_tasks" { return self.run_write_tasks(..) }
if name == TASK_TOOL     { return self.run_task(..)        }
```

These three return before the class is even looked up. They are exempt from the
plan gate, from the permission prompt, from checkpointing **and from schema
validation** (see finding C). The justification in the code is that none of
them has an effect outside the conversation: a modal, a checklist, and a child
agent whose own tool set is intersected down to the read-only tools.

`task` is the one worth watching. It is exempt because its *child* is
constrained, not because delegation is inherently harmless — so every
constraint on the child (read-only tools, `PermissionPolicy::Ask`, no inherited
grants, `plan_gated` copied down, refused permission and question channels) is
load-bearing for the parent's exemption to be true.

### 2. `plan_gated` — unconditional, and above everything

```rust
if self.plan_gated && class != PermissionClass::ReadOnly { return blocked }
```

Nothing overrides it. Not `PermissionPolicy::Skip`, not a session grant, not
`scratch_scoped`, not a hook. It is checked before all of them and it does not
consult any of them. `/plan approve` or `/plan reject` is the only exit.

This is the one gate in the system with no bypass at all, and it is the reason
the order matters: every later rung is written on the assumption that this one
already ran.

### 3. `PreToolUse` hook — deny-only, between the gate and the prompt

See [hooks.md](hooks.md) for the contract. Its position is argued at the bottom
of this document. In one line: it can deny, and it can rewrite `A`; it can
never allow something rungs 2, 4 or 5 would have refused.

### 4. Whether to ask at all

```rust
let needs_prompt = class != PermissionClass::ReadOnly
    && !self.allowed_session_tools.contains(name)
    && !self.permission_policy.auto_allows(class)
    && !self.tools.scratch_scoped(name, &input, &self.tool_ctx);
```

Four independent reasons not to prompt, OR'd together (the `&&` chain is the
negation of an OR). Any one of them is enough:

- **`class == ReadOnly`** — a property of the tool, fixed at registration.
  `read_file`, `list_dir`, `glob`, `grep`, `web_search`, `task`.
- **`allowed_session_tools`** — the user pressed "allow always" earlier, or
  headless answered `AllowSession` because `--allowed-tools` named it. Per tool
  *name*, never per path or per command.
- **`permission_policy.auto_allows(class)`** — `Session` auto-allows
  `Mutating`, `Skip` auto-allows everything.
- **`scratch_scoped`** — the tool vouches that this specific call is confined
  to `.smith/scratch/<session>/`.

None of these is stronger than another; they are all equally sufficient. That
is worth stating because it is the source of most of the surprising cases
below.

### 5. The answer

`Deny` → `ToolResult::error("User denied permission to run this tool.")`.
`AllowOnce` → proceed. `AllowSession` → proceed *and* insert into
`allowed_session_tools`, which is rung 4's second clause forever after.

In headless there is no modal: `headless.rs` answers the channel from
`--allowed-tools`, and answers `AllowSession` rather than `AllowOnce` because
a flag cannot change mid-run. Headless also forces `PermissionPolicy::Ask`
regardless of saved config, so rung 4's third clause is always false there —
a stored `skip` cannot pre-empt the flag.

### 6–7. After authorization: shape, then substance

`ToolRegistry::execute` validates `A` against the schema the model was shown,
then the tool runs its own checks. `fs_tools::ReadSet` is the notable one: a
`write_file` that would replace an existing file is refused unless `read_file`
has shown the model those exact bytes this session. It is not an authorization
gate — the user already said yes — it is a correctness gate against blind
overwrites, and it lives inside the tool because it needs the file's current
hash.

`dispatch_tool` also snapshots (`checkpoint_before`) between rung 5 and rung 6,
so a refused call never leaves an object behind. Checkpointing can never *fail*
a call; it degrades to an advisory line.

### The concurrent path

`run_turn` groups read-only calls and runs them through `run_concurrent_group`,
which calls `dispatch_tool` directly with a hardcoded
`PermissionClass::ReadOnly` — **rungs 2, 4 and 5 are skipped entirely**. That
is sound only because `is_concurrency_safe` admits nothing but tools whose
class is exactly `ReadOnly` (an unknown tool reports `None` and is excluded),
and for those, those rungs are no-ops anyway. It is a real second path through
the system, and anything added to rungs 2–5 must be added to both or explicitly
argued not to apply. `PreToolUse` is added to both.

---

## Where two of them disagree

**A session grant outranks a tightened policy.** Grant `run_bash` with "allow
always", then `/permission ask`. `run_bash` still never prompts:
`allowed_session_tools` is checked before `permission_policy`, and
`set_permission_policy` does not clear grants. There is no way to revoke a
grant short of restarting — and `switch_model` deliberately carries grants
across a provider rebuild. Defensible (re-prompting for work already signed off
is its own harm) but it means "the policy is `ask`" is not the same claim as
"you will be asked".

**A grant is per name, not per call.** "Allow always" on `run_bash` after
seeing `ls` authorizes every future shell command in the session. The modal
shows a specific command; the grant it records does not.

**`Skip` does not skip the plan gate.** By design, and the more useful framing
is the converse: `/plan` is the only thing in the system a user under `skip`
still cannot talk their way past.

**`scratch_scoped` beats the policy but loses to the plan gate.** A scratch
write under `/permission ask` runs silently; the same write while a plan is
pending is blocked. Both are deliberate — the check is placed after the gate
precisely so scratch writes stay side effects.

**A read-only tool is not a harmless tool.** `web_search` reaches the network
and `task` spawns a whole agent with its own token spend; both are `ReadOnly`,
so both are invisible to rungs 4 and 5. `web_fetch` is the counter-example done
right: it is `Mutating` despite writing nothing locally, because a model-chosen
URL is an exfiltration primitive. The class is about *authority*, not about
whether bytes hit the disk — and two tools in the registry are classified as
though it were about bytes.

**An unknown tool is `Dangerous`.** `permission_class` returns `None`, which
becomes `Dangerous`, so a hallucinated tool name can be blocked by the plan
gate or raise a permission modal for a tool that does not exist. `execute` then
answers "unknown tool". Fail-safe, but the modal is a lie.

**A forbidden tool inside a subagent is `ReadOnly`.**
`RestrictedTools::permission_class` reports `ReadOnly` for names it will
refuse, deliberately: the refusal comes from `execute`, and reporting `None`
would open a permission modal on a UI with nobody to answer it. So inside a
child, "class" stops describing the tool and starts describing "what refusal
you will get".

---

## Findings: where the code does not match a defensible model

### A. A subagent's reads unlock the parent's overwrites

`ReadSet` keys its entries on `(session_id, path)` and says so:

> Keyed by session as well as path, so a second session sharing one registry
> (a subagent, a resumed run) starts out knowing nothing rather than
> inheriting another session's reads.

A subagent is built with `self.tool_ctx.clone()` (`Agent::run_task`), so its
`session_id` is *identical* to the parent's. The intent stated in that comment
is not achieved: parent and child share one `ReadSet` namespace.

The effect is not cosmetic. The read-before-overwrite gate exists so the model
cannot replace a file it has not seen. A child's `read_file` records the file
as known — and the child's transcript is **discarded**; the parent only ever
sees a summary report. So the parent can now `write_file` over a file whose
contents no agent in the conversation still has. Delegation launders the gate.

**Fixed.** Not by deriving a session id, which was the obvious move and the
wrong one: `session_id` also names the staging directory, the scratch
directory and the checkpoint stream `/rewind` walks, and a child that ever
gains a write tool would have quietly put its checkpoints somewhere the
parent's `/rewind` does not look. The two identities are genuinely different,
so `ToolContext` now carries both — `session_id` for on-disk state, `reader_id`
for what has been read — and `ToolContext::for_delegate` changes only the
second. `ReadSet` keys on `reader_id`, which defaults to the session id, so an
ordinary session behaves exactly as before. Two subagents in one turn do not
unlock each other either.

### B. `--allowed-tools` is not deny-by-default

The claim (`CLAUDE.md`, and the flag's own help): headless permissions deny by
default and `--allowed-tools` is the only gate. What the code does:
`headless.rs` answers the *permission channel*. A call that never reaches the
channel is never checked against the flag.

Calls that never reach the channel:

- every `ReadOnly` tool — `read_file`, `list_dir`, `glob`, `grep`,
  `web_search`, and `task`, which spawns an entire child agent that spends the
  user's tokens;
- `ask_user` and `write_tasks` (intercepted; `ask_user` is separately refused,
  so this one is harmless);
- **any `write_file` / `edit_file` / `multi_edit` whose path is inside
  `.smith/scratch/<session>/`** — `scratch_scoped` suppresses the prompt at
  rung 4, so a `Mutating` call runs in a headless job that listed no tools at
  all.

**Fixed, for the two cases that mattered.**

Both exemptions were justified by the same thing: not interrupting a human.
The scratch exemption exists because prompting for throwaway files is what
pushes the model into writing them into the project instead; `task` skips the
gate because the user is watching and the child can only read. Neither
argument survives when nobody is at the terminal — the channel answers
instantly from a list, so there is no friction to spare — while both left a
call running in a job that named no tools at all.

`Agent::with_unattended(true)`, set by the headless frontend, therefore turns
both off:

- a scratch-confined `Mutating` call goes to the channel like any other, so
  `--allowed-tools` decides it;
- `task` must be named. It is classed `ReadOnly` because a child's own tools
  are, which is right interactively and wrong unattended: "spawn a whole agent
  and spend the user's money" is not what a reader expects `--allowed-tools`
  to leave open.

Carried across `/model` alongside the limits and the redactor, for the same
reason: a switch that quietly re-enabled the exemptions would be the least
visible way to lose the only gate a headless run has.

The remaining `ReadOnly` tools — `read_file`, `list_dir`, `glob`, `grep`,
`web_search` — still run unlisted, and that is intended rather than
outstanding: the flag's own help says "tools a non-interactive run may use
**beyond the read-only ones**". What was wrong was `CLAUDE.md` calling it "the
only gate"; the accurate statement is that nothing that writes, runs a
command, or spawns an agent happens without it.

### C. Three tools are never schema-checked

`ToolRegistry::execute` calls itself "the one place a tool call is checked
against the schema the model was shown", and argues it belongs there because
"it is the choke point every call passes through, so a tool added tomorrow is
covered". It is not that choke point. `ask_user`, `write_tasks` and `task`
return from `run_one_tool` before `execute` is reached, and each re-implements
ad-hoc argument parsing (`str_arg`, `parse_tasks`, the option triple). Today
their hand-written checks are decent, so this was a latent bug rather than a
live one — but the invariant the comment stated was false, and the next
intercepted tool inherits the false version.

**Fixed.** `run_one_tool` validates the intercepted three against the same
published schema immediately before intercepting them, and the registry's own
doc comment now says "dispatched call" rather than claiming to be the only
place. The list lives in one constant, `INTERCEPTED_TOOLS`, because two things
have to agree about it: the interception arms and the check that has to run
ahead of them.

The `PreToolUse` hook implementation already closed part of this: rewritten
arguments are validated at the hook site through `ToolExecutor::validate_input`.
That covered a hook's rewrite, never the model's original call.

---

## Where `PreToolUse` sits, and why

**Rung 3: after the plan gate, before the permission prompt. Deny-only.**

The brief poses it exactly right — a hook that can override the plan gate is a
hole, and a hook that runs after the prompt cannot prevent the prompt. There is
one position that avoids both, and one property that has to hold alongside it.

**The property: a hook can only subtract.** A `PreToolUse` response of
`{"decision": "allow"}` means "no objection", never "let it through". The call
still faces rungs 4, 5, 6 and 7 unchanged. This is what makes the position
argument tractable at all: once a hook cannot grant, the only thing its
position decides is *what it can prevent* and *what it can see*.

**Why not above the plan gate.** A hook that runs before rung 2 would fire for
calls that cannot possibly execute. Nothing is gained — it cannot allow them —
and two things are lost: the user's machine spawns a process per blocked call,
and any hook that logs (the most common kind) records attempts that were never
live. It also puts an `allow` decision syntactically upstream of the one gate
that has no bypass, which is an invitation to someone later wiring it up as an
override. Below the gate, that mistake is not even expressible.

**Why not below the permission prompt.** Two reasons, both fatal:

1. *A hook must be able to prevent the prompt.* The main thing users want from
   `PreToolUse` is "never let this agent touch `.env`, and don't ask me". A
   hook that runs after the modal has already interrupted them has failed at
   its only job — and worse, it teaches the user to approve things that are
   then silently denied, which is exactly how people learn to click through
   modals.
2. *A rewrite has to be visible in the modal.* `format_permission_detail`
   builds the prompt text from the arguments. If the hook rewrote them
   afterwards, the user would approve one command and a different one would
   run. Any hook that rewrites arguments and runs after the prompt is a
   confused-deputy generator. Placing it at rung 3 makes the modal show what
   will actually happen.

**Why above rung 4 specifically, not merely above rung 5.** Rung 4 is where
`allowed_session_tools`, `PermissionPolicy::Skip` and `scratch_scoped` decide
there will be no prompt at all. A hook placed between 4 and 5 would be skipped
entirely for exactly those calls — that is, it would stop working the moment
the user set `/permission skip`, which is precisely when they most want a
backstop. At rung 3 the hook runs for every call regardless of policy, grants,
or scratch confinement.

**The one exception to "after the plan gate".** `ask_user`, `write_tasks` and
`task` are intercepted at rung 1 and never reach rung 2, so for them the hook
is simply the first gate there is. That is not a special case in the ordering
— there is no plan gate for those calls to be after — and it is what lets a
policy hook see `task`, the one intercepted tool whose effects are not confined
to the UI.

**The resulting total order** (a deny at any point ends the call):

```
visible tools → interception → plan gate → PreToolUse hook
    → prompt decision → prompt → schema → tool's own checks → execute
    → PostToolUse hook → redact → result
```

Read as a lattice of authority: rungs 0–4 and 6–7 can only ever remove
authority. Rung 5 is the only rung that adds any, and it is the only one that
requires a human (or, in headless, an explicit flag) to speak. A hook is on the
subtracting side, which is why it does not need to be trusted with anything —
the worst a hostile hook *config* can do is refuse to let the agent work, and
the worst a hostile hook *output* can do is bounded by the sanitisation in
[hooks.md](hooks.md).
