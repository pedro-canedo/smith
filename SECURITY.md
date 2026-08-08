# Security policy

## Reporting a vulnerability

**Please do not open a public issue.** Use GitHub's private reporting:

> [Report a vulnerability](https://github.com/pedro-canedo/smith/security/advisories/new)

That opens a private thread with the maintainer. If the form is unavailable
to you, email `devpedrocanedo@gmail.com` with `smith security` in the
subject.

Expect an acknowledgement within a week. Please give a fix a reasonable
window before disclosing publicly; if the report goes unanswered for 30
days, disclose.

Supported: the latest release. There are no maintained older branches.

## What counts as a vulnerability here

smith is a coding agent. It reads and writes files, runs shell commands,
talks to model providers, and — when the web console is enabled — serves a
local HTTP endpoint. The interesting failures are ones where **a boundary
that is supposed to hold does not**:

- A tool escaping the project directory: a path, symlink or `..` that
  resolves outside the jail (`smith-tools`'s `resolve`, `smith-config`'s
  `@import`, the extension loader's discovery jail).
- **Authorization bypass** — anything that runs a `Mutating` or `Dangerous`
  tool without the prompt or grant `docs/authorization.md` says is required,
  or that survives an unapproved plan gate.
- **Prompt injection that reaches a capability.** Text in a file, a web page,
  a tool result or an MCP server steering the agent into running a command or
  writing a file. Content that is merely *quoted back* is working as
  designed; content that *acts* is not. See the fencing in
  `smith-mcp`'s `untrusted` and `web_fetch`.
- **Secret disclosure**: an API key reaching a transcript, a log, a session
  database, a tool result, or the web console. `smith-core`'s `Redactor`
  exists for this.
- **Web console / config server**: anything reaching either endpoint without
  the per-run token, from another origin, or from another host — the
  predicates in `smith-cli`'s `webguard`.
- **Release integrity**: a way to make `smith update` install something the
  maintainer did not publish.

## What is not a vulnerability

- **A tool doing what it was approved to do.** `run_bash` runs arbitrary
  commands; that is the feature. A user approving a destructive command is a
  user decision, not a bypass.
- **`--permission skip` behaving as documented.** It exists to waive prompts.
- **A model saying something wrong.** Model behaviour is measured in
  `evals/`, not treated as a security boundary.
- **Binding the config server or web console to loopback being "insecure"**
  because another local user could reach it. Loopback plus a per-run token is
  the stated model; a report needs to defeat the token, the `Host` check, or
  the origin checks.

A report that includes the smallest reproduction you can manage — ideally a
failing test — will be fixed considerably faster than one that does not.
