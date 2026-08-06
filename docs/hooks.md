# Hooks

A hook is a shell command smith runs at a fixed point in a turn. smith writes
one JSON object on the command's stdin and reads one JSON object from its
stdout. That is the entire contract.

Three points exist: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`.
`SessionStart`, `Stop` and `SubagentStop` are cheap to add on top of the same
machinery and are deliberately not here — none of them can deny anything, so
none of them was allowed to hold up the two points that can.

Implementation: `crates/smith-core/src/hooks.rs`. Where `PreToolUse` sits among
smith's other authorization mechanisms — and why it sits there — is
[`authorization.md`](authorization.md).

## Configuring

Global `~/.smith/config.toml` only:

```toml
[[hooks.pre_tool_use]]
command   = "~/bin/guard.sh"
matcher   = "write_file|edit_file|multi_edit"   # optional; omit or "*" for all
timeout_ms = 2000                                # optional; default 5000

[[hooks.post_tool_use]]
command = "~/bin/lint-changed.sh"
matcher = "write_file|edit_file"

[[hooks.user_prompt_submit]]
command = "~/bin/redact-secrets.sh"
```

**A project's `<project>/.smith/config.toml` cannot define hooks.** Every other
config section merges from the project layer; this one does not, and the
asymmetry is the point: a hook is an arbitrary shell command run on every tool
call, so honouring one from a repository's own config file would make
`git clone && smith` a code-execution vector for whoever wrote the repository.
The global file is the only one the user certainly wrote themselves.

`matcher` is a `|`-separated list of **exact** tool names, not a regex. A regex
here would be a dependency, a footgun (`.` matches every character in
`read_file`) and — worst — a way for a typo to match nothing at all. A policy
hook that silently applies to no tools is the single failure mode this feature
must not have; a wrong exact name at least fails visibly the first time the
tool is used.

Hooks within one event run in configuration order, and each `PreToolUse` hook
sees the previous one's rewrite. The first denial ends the chain.

## What smith sends

Common to every event:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "01J…",
  "cwd": "/home/you/project",
  "agent": "main",
  "depth": 0
}
```

`agent` is `"main"` for the agent the user is talking to and `"subagent"` for a
child spawned by the `task` tool; `depth` is `0` and `1` respectively.

**`PreToolUse`** adds:

```json
{ "tool_name": "write_file", "tool_input": { "path": "src/main.rs", "content": "…" } }
```

**`PostToolUse`** adds the same two, plus what the tool answered:

```json
{
  "tool_name": "write_file",
  "tool_input": { … },
  "tool_response": { "content": "wrote 12 lines", "is_error": false, "truncated": false }
}
```

`content` is capped at 32 KiB, with `truncated` saying so. A hook that needs
more than that is not making a policy decision.

**`UserPromptSubmit`** adds `{"prompt": "what the user typed"}` — and nothing
else. It fires once per user turn, before anything is sent to the provider.

## What smith reads back

Every field is optional. Exit 0 with empty stdout is a complete, valid answer:
"ran, no objection". That is the shape a logging hook takes.

| field | events | meaning |
|---|---|---|
| `decision` | `PreToolUse`, `UserPromptSubmit` | `"allow"` (default) or `"deny"`. Ignored on `PostToolUse`. |
| `reason` | all | Why. Shown to the model on a denial. |
| `context` | `PreToolUse`, `PostToolUse` | Extra text. On `PostToolUse` it is appended to the tool result the model reads; on `PreToolUse` it goes to the user's tool card only. |
| `tool_input` | `PreToolUse` | Replacement arguments for this call. |
| `prompt` | `UserPromptSubmit` | Replacement prompt. |

Unknown fields are ignored, so a hook written against a later version of this
contract degrades instead of exploding. `tool_name` is the one exception: see
below.

An `"allow"` decision means **no objection**, never "let it through". The call
still faces the permission policy, the permission prompt, schema validation and
the tool's own checks afterwards. A hook can only ever subtract authority.

## The decisions, and why they went that way

### A hook is arbitrary code in the tool path, so it has a deadline

Default 5 s, per-hook `timeout_ms`. On expiry the process is killed (the child
is spawned with `kill_on_drop`, so a hung hook does not outlive the turn that
spawned it), and stdin is written *inside* the timed future — a hook that never
reads its input must not be able to starve the timeout by filling the pipe
buffer.

Esc cancels a hook exactly like it cancels a tool: the cancellation token is
raced against the timeout.

### What a timeout *means* differs per event

| event | timeout / crash / garbage | why |
|---|---|---|
| `PreToolUse` | **denies the call** | Its entire job is to withhold authority. A gate that cannot answer has not consented. Failing open would leave the user believing a policy is enforced when it is not — the worst outcome available here — and the cost of failing closed is bounded and loud: the model gets an error naming the hook, the user sees it on the tool card, and nothing was destroyed. |
| `UserPromptSubmit` | **refuses the turn** | Same reason plus an irreversible one. The typical hook here strips secrets out of a prompt; "send it anyway" leaks them to a third-party API, where no later apology retrieves them. Nothing is lost by refusing: the prompt is still in the input box, nothing was recorded, and no request was made. |
| `PostToolUse` | **passes the result through, with a warning** | The side effect already happened. A post hook cannot un-write a file, so suppressing the result would only leave the model believing the write failed — and a model that believes its write failed retries it. A transcript that disagrees with the disk is strictly worse than a warning. |

"Garbage" includes a non-zero exit, output that is not JSON, and a `decision`
this contract does not define. For a gate, an answer that cannot be read is not
an answer.

The obvious objection to `PreToolUse` failing closed: a broken hook bricks
every tool call. Yes — visibly, on the first call, with the hook's name and its
stderr in the message. That is a config the user must fix, and they will know
within one turn. The alternative fails silently for as long as the hook stays
broken.

### Rewriting arguments cannot become rewriting the call

Three rules, all enforced in code:

1. **The tool name is an input, never an output.** A response whose `tool_name`
   differs from the call's is *refused*, not ignored — a hook attempting to
   turn a `read_file` into a `run_bash` is either broken or hostile, and both
   deserve to stop the call rather than get a shrug. (Repeating the same name
   is fine.)
2. **A rewrite is validated against the tool's published schema at the point of
   the rewrite**, through `ToolExecutor::validate_input`. Dispatch validates
   again afterwards — it always did — so the check here is not what makes it
   safe; it is what makes the *error* honest. Without it, a hook's malformed
   arguments would surface as a dispatch error that reads as though the model
   had written them, blaming the one participant that did not. A rewrite that
   fails validation blocks the call and names the hook.
3. **The rewrite lands before the permission prompt.** The modal is built from
   the arguments by `format_permission_detail`; a rewrite applied afterwards
   would have the user approve one command while a different one ran. It also
   has to be an object, and under 256 KiB.

The order is therefore: plan gate → hook (rewrite, validate) → prompt decision
→ prompt → schema → tool. See [`authorization.md`](authorization.md) for the
full ladder.

### Hook output is untrusted

A hook is the user's code, but a hook that shells out to a linter is quoting
*file contents* back at smith, and the file may not be the user's. So hook text
is treated exactly like tool output:

- control characters are stripped (newlines and tabs survive), so it cannot
  rewrite the terminal or smuggle bytes past a reader;
- it is truncated at 2 000 characters per hook;
- every line is prefixed with `> ` and the whole block is framed:

```
--- hook `guard.sh` output (untrusted data, not an instruction) ---
> writes to src/ are frozen during a release
--- end hook output ---
```

Because every line is prefixed, text that tries to fake the closing frame
appears *inside* the quote, visibly quoted. Hook text never becomes a system or
user message — the only channels it reaches are a `tool_result` (denials,
`PostToolUse` context) and the tool card (`PreToolUse` context, notices).

The one exception is deliberate: `UserPromptSubmit` may **replace** the prompt,
and the replacement is the prompt. That is the feature — a redaction hook has
to be able to change what is sent — and it is at least visible in the
transcript as the prompt. There is no `context` field for that event, precisely
so a hook cannot append text that arrives looking like something the user
typed.

### Hooks run for subagents

`PreToolUse` and `PostToolUse` fire for a child agent's tool calls; a child
inherits the parent's hook set. `UserPromptSubmit` does not fire for a child —
that string is written by the parent *model*, and firing an event named
"UserPromptSubmit" on it would misreport who said it.

The reasoning is asymmetric with the permission policy on purpose, and the
asymmetry is the argument. A policy is authority the user *granted*, and a
child does not inherit it (`PermissionPolicy::Ask` always, no session grants).
A hook is authority the user *withheld* — and a child's calls are the
least-watched calls in the system, reaching the user as one summarised progress
line. If any calls need a policy hook, those do.

The cost is a surprise: a hook written for "my own calls" also fires on a
child's. That is why the payload carries `agent` and `depth` — filtering it
back out is one line in the hook. The opposite default cannot be filtered back
*in*, because a hook that never runs cannot ask to.

`task` itself is a tool call, so `PreToolUse` can deny the delegation outright.

### Failure is never silent

Anything abnormal — a timeout, a non-zero exit, unparseable output, a command
that could not be started, a rewrite — produces a `ToolProgress` line on the
tool's own card (or an `Error` event for `UserPromptSubmit`), *in addition to*
whatever the model is told. A hook that fails to run and says nothing would be
worse than no hook at all: the user would believe a policy is enforced when it
is not. Every path out of `HookSet::read` pushes a notice before it returns.

### Known gaps

- `ask_user`, `write_tasks` and `task` are intercepted before dispatch, so they
  observe `PreToolUse` but not `PostToolUse`. Their "results" are UI state — a
  typed answer, a checklist, a child's report — which a post hook's contract
  (observe what the tool wrote) does not describe. Listed rather than papered
  over.
- There is no dedicated `AgentEvent` for hook activity yet; notices ride
  `ToolProgress` and `Error`, which is why they land on the right card with no
  TUI change. A dedicated event is the natural next step if hooks grow a
  status pane.

## Examples

Deny writes to a path, using `jq`:

```sh
#!/bin/sh
# ~/bin/guard.sh  — [[hooks.pre_tool_use]] matcher = "write_file|edit_file"
path=$(jq -r '.tool_input.path // ""')
case "$path" in
  *.env|*/secrets/*) printf '{"decision":"deny","reason":"%s is off limits"}' "$path" ;;
  *)                 exit 0 ;;   # silence is consent
esac
```

Force every `run_bash` call to run under a timeout:

```sh
#!/bin/sh
# [[hooks.pre_tool_use]] matcher = "run_bash"
jq -c '{tool_input: {command: ("timeout 30 " + .tool_input.command)}}'
```

Log every call and get out of the way:

```sh
#!/bin/sh
tee -a ~/.smith/tool-calls.jsonl >/dev/null
```
