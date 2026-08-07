//! The slash commands, from both of the blocks they used to sit in.

use smith_core::{Action, AgentPhase, McpCommand, PermissionPolicy};

use super::chatline::{ChatLine, ChatRole};
use super::chrome::{row, Overlay, LOG_PANEL_TITLE};
use super::modal::Modal;
use super::App;

impl App {
    /// Dispatches `/name args`.
    ///
    /// Built-ins are matched first and custom commands only reach the fallback
    /// arm, so a file in a cloned repository cannot change what `/clear` does
    /// however the registry was built. See `crate::slash`.
    pub(crate) fn run_slash_command(&mut self, command: &str) -> Option<Action> {
        let mut parts = command.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();

        match name {
            "clear" => {
                self.lines.clear();
                None
            }
            "help" | "" => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    "commands: /clear (clear the visible transcript), /model [<name>|<provider>/<name>] [--save] (show or switch model), /permission [ask|session|skip] [--save] (show or set the tool permission policy), /usage (session token/cost/tool-call summary), /plan <task>|approve|reject (plan before executing), /goal [<description>|clear] (set, show, or clear the session goal), /loop [<N>] <task>|goal (repeat a task until done, N iterations, or Esc), /compact (summarise old history to reclaim context), /remember <note> (append a standing note to this project's SMITH.md), /mcp [prompt [<server>] <name> [key=value ...]] (list MCP servers, or run one's prompt template),/rewind [<turn>] [confirm] [--force] (undo a turn's file writes — shows the plan first; does NOT undo anything run_bash did), /help (this message)",
                ));
                self.show_custom_commands();
                None
            }
            "model" => self.run_model_command(args),
            "permission" => self.run_permission_command(args),
            "usage" => {
                self.show_usage();
                None
            }
            "mcp" => self.run_mcp_command(args),
            "queue" => self.run_queue_command(args),
            "plan" => self.run_plan_command(args),
            "rewind" => self.run_rewind_command(args),
            "goal" => self.run_goal_command(args),
            "loop" => self.run_loop_command(args),
            "compact" => {
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "can't compact mid-turn — wait for the current turn to finish",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    "compacting the conversation…",
                ));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                Some(Action::Compact)
            }
            "remember" => {
                let note = args.trim();
                if note.is_empty() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "usage: /remember <note> — appends a standing instruction to this project's SMITH.md",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("remembered: {note}"),
                ));
                Some(Action::Remember(note.to_string()))
            }
            other => self.run_custom_command(other, args),
        }
    }

    /// Lists custom commands under `/help`, with the file each came from.
    ///
    /// The path is not decoration: `/deploy` doing something surprising is a
    /// question about *which file* defines it, and a user who cloned the repo
    /// has no other way to find out.
    fn show_custom_commands(&mut self) {
        let custom = self.commands.custom();
        if custom.is_empty() {
            return;
        }
        let listed: Vec<String> = custom
            .commands()
            .iter()
            .map(|c| format!("/{} ({}) — {}", c.name, c.description, c.source.display()))
            .collect();
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("custom commands: {}", listed.join(", ")),
        ));
    }

    /// A command loaded from `.smith/commands/` or `~/.smith/commands/`.
    ///
    /// The expansion is submitted as an ordinary user message — there is no
    /// new `Action` and no capability a custom command has that typing the
    /// same prose would not.
    ///
    /// **The expanded body goes into the transcript, not the `/name`.** That
    /// is the one thing that makes a prompt from a file safe to run: the user
    /// sees exactly what was sent, in the same breath as it is sent, so a
    /// command that is not what they expected is visible rather than inferred.
    /// A system line above it names the file, so a project command is
    /// attributable at a glance.
    fn run_custom_command(&mut self, name: &str, args: &str) -> Option<Action> {
        // Lowercased because command names are normalised at load time; the
        // user typing `/Deploy` should reach the same file `/deploy` does.
        let lowered = name.to_ascii_lowercase();
        let Some(command) = self.commands.custom().get(&lowered) else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("unknown command: /{name}"),
            ));
            return None;
        };

        let source = command.source.display().to_string();
        let prompt = match command.render(args) {
            Ok(prompt) => prompt,
            // A missing `$1` refuses rather than expanding to nothing — see
            // `CustomCommand::render`. The message names the placeholders.
            Err(problem) => {
                self.lines.push(ChatLine::new(ChatRole::System, problem));
                return None;
            }
        };
        if self.waiting_on_assistant {
            return None;
        }

        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("/{lowered} — from {source}"),
        ));
        self.lines
            .push(ChatLine::new(ChatRole::User, prompt.clone()));
        self.waiting_on_assistant = true;
        self.phase = AgentPhase::Thinking;
        self.in_flight_text = None;
        self.metrics.begin_turn();
        self.request_count += 1;
        Some(Action::SubmitMessage(prompt))
    }

    /// `/mcp` — connected servers — and `/mcp prompt [<server>] <name>
    /// [key=value ...]`, which runs a prompt template one of them supplies.
    ///
    /// The subcommand lives here rather than as its own `/`-command because a
    /// server-supplied prompt is not smith's own command: keeping it behind
    /// `/mcp` says where it came from every time it is typed, and leaves the
    /// top-level namespace to the frontend that owns it.
    fn run_mcp_command(&mut self, args: &str) -> Option<Action> {
        let mut tokens = args.split_whitespace();
        match tokens.next() {
            None => Some(Action::Mcp(McpCommand::Status)),
            Some("prompt") => {
                let mut positional: Vec<&str> = Vec::new();
                let mut arguments: Vec<(String, String)> = Vec::new();
                for token in tokens {
                    match token.split_once('=') {
                        Some((k, v)) => arguments.push((k.to_string(), v.to_string())),
                        None => positional.push(token),
                    }
                }
                // One bare word is a prompt name; two are a server and a
                // prompt name. Guessing between them is only ambiguous if a
                // prompt is named after a server, and then the two-word form
                // is the way to say which you meant.
                let (server, name) = match positional.as_slice() {
                    [name] => (None, name.to_string()),
                    [server, name] => (Some(server.to_string()), name.to_string()),
                    _ => {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "usage: /mcp prompt [<server>] <name> [key=value ...]",
                        ));
                        return None;
                    }
                };
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "can't run a prompt mid-turn — wait for the current turn to finish",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    match &server {
                        Some(s) => format!("running MCP prompt `{name}` from `{s}`…"),
                        None => format!("running MCP prompt `{name}`…"),
                    },
                ));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                self.in_flight_text = None;
                self.metrics.begin_turn();
                self.request_count += 1;
                Some(Action::Mcp(McpCommand::Prompt {
                    server,
                    name,
                    arguments,
                }))
            }
            Some(other) => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!(
                        "unknown /mcp subcommand: {other} — try /mcp, or \
                         /mcp prompt [<server>] <name> [key=value ...]"
                    ),
                ));
                None
            }
        }
    }

    /// `/rewind [<turn>] [confirm] [--force]`.
    ///
    /// Two steps on purpose. Restoring files overwrites whatever is on disk
    /// now, which is the one thing in smith that can destroy work the user did
    /// themselves — so the bare command only ever *describes* what it would
    /// do, and `confirm` is a separate keystroke the user makes after reading
    /// it.
    fn run_rewind_command(&mut self, args: &str) -> Option<Action> {
        let mut turn = None;
        let mut apply = false;
        let mut force = false;
        for token in args.split_whitespace() {
            match token {
                "confirm" | "--confirm" => apply = true,
                "--force" | "-f" => force = true,
                other => match other.parse::<u64>() {
                    Ok(n) => turn = Some(n),
                    Err(_) => {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "usage: /rewind [<turn>] [confirm] [--force] — with no `confirm` it \
                             only shows what it would restore",
                        ));
                        return None;
                    }
                },
            }
        }

        // The checkpoint of a turn still in flight is incomplete, and undoing
        // half of it would be worse than not offering to.
        if self.waiting_on_assistant {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "can't rewind mid-turn — press Esc to stop the current turn first",
            ));
            return None;
        }

        Some(Action::Rewind { turn, apply, force })
    }

    fn run_model_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() {
            // Ask for the catalogue; the picker opens when it arrives. The
            // frontend cannot fetch it — `smith-tui` does not depend on
            // `smith-provider`, and it is a network call besides.
            self.lines
                .push(ChatLine::new(ChatRole::System, "reading the model list…"));
            return Some(Action::ListModels);
        }
        if args.eq_ignore_ascii_case("list") {
            self.show_model_info();
            return None;
        }

        let mut save = false;
        let mut spec_tokens = Vec::new();
        for token in args.split_whitespace() {
            if token == "--save" {
                save = true;
            } else {
                spec_tokens.push(token);
            }
        }

        let Some(spec) = spec_tokens.first() else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /model <name> [--save]  or  /model <provider>/<name> [--save]",
            ));
            return None;
        };

        let (provider, model) = match spec.split_once('/') {
            Some((p, m)) => {
                if smith_store::is_known_provider(p) {
                    (Some(p.to_string()), m.to_string())
                } else if matches!(self.provider_label.as_str(), "openrouter" | "9router") {
                    // Model ids on these providers contain slashes
                    // (`qwen/qwen3-coder:free`), so under an active gateway
                    // session an unknown prefix is a *namespace*, not a typo'd
                    // provider — `/model qwen/qwen3-coder:free` must work.
                    // Under any other provider the old strictness stands: a
                    // typo must not silently become a model name.
                    (None, spec.to_string())
                } else {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!(
                            "unknown provider: {p} (expected anthropic, openai, openrouter, \
                             9router, or ollama)"
                        ),
                    ));
                    return None;
                }
            }
            None => (None, spec.to_string()),
        };

        if model.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /model <name> [--save]  or  /model <provider>/<name> [--save]",
            ));
            return None;
        }

        Some(Action::SwitchModel {
            provider,
            model,
            save,
        })
    }

    fn show_model_info(&mut self) {
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("current: {}/{}", self.provider_label, self.model_label),
        ));
        let known = smith_store::known_models(&self.provider_label);
        if !known.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("known {} models: {}", self.provider_label, known.join(", ")),
            ));
        }
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "switch with /model <name>, or /model <provider>/<name>; add --save to persist",
        ));
    }

    fn run_permission_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() {
            self.show_permission_info();
            return None;
        }

        let mut save = false;
        let mut mode_tokens = Vec::new();
        for token in args.split_whitespace() {
            if token == "--save" {
                save = true;
            } else {
                mode_tokens.push(token);
            }
        }

        let Some(mode) = mode_tokens.first() else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /permission <ask|session|skip> [--save]",
            ));
            return None;
        };

        let Some(policy) = PermissionPolicy::parse(mode) else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("unknown mode: {mode} (expected ask, session, or skip)"),
            ));
            return None;
        };

        if policy == PermissionPolicy::Skip {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "⚠ skip mode auto-allows every tool call, including shell commands, with no confirmation of any kind.",
            ));
        }

        Some(Action::SetPermissionPolicy { policy, save })
    }

    fn show_permission_info(&mut self) {
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("current: {}", self.permission_policy.as_str()),
        ));
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "modes: ask (always prompt, default), session (auto-allow file writes/edits, still prompts for shell/MCP tools), skip/yolo (auto-allow everything, no prompts)",
        ));
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "switch with /permission <mode>; add --save to persist",
        ));
    }

    /// `/usage` — the session's accounting, as a table.
    ///
    /// The cost row is whatever the agent last reported and is never derived
    /// from `self.usage`: those tokens are a running total across the whole
    /// session, while the cost is a sum of per-turn figures priced when each
    /// turn ran. Multiplying today's price by the lifetime token count is the
    /// bug acceptance criterion #4 exists to catch.
    fn show_usage(&mut self) {
        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        let cost = match self.session_cost {
            Some((usd, _)) => format!("~${usd:.4}"),
            None => "n/a".to_string(),
        };

        let mut rows = vec![
            row(["requests", &self.request_count.to_string()]),
            row(["tool calls", &self.tool_call_count.to_string()]),
            row(["input tokens", &self.usage.input_tokens.to_string()]),
            row(["output tokens", &self.usage.output_tokens.to_string()]),
            row(["total tokens", &total_tokens.to_string()]),
            row(["cost (est.)", &cost]),
        ];
        if self.usage.cache_read > 0 || self.usage.cache_write > 0 {
            rows.insert(4, row(["cache read", &self.usage.cache_read.to_string()]));
            rows.insert(5, row(["cache write", &self.usage.cache_write.to_string()]));
        }

        let mut footer = vec![format!("{}/{}", self.provider_label, self.model_label)];
        match self.session_cost {
            Some((_, unpriced)) if unpriced > 0 => footer.push(format!(
                "{unpriced} turn(s) ran on a model with no known price and are not in the total"
            )),
            None => footer.push(format!(
                "no pricing data for {}/{}",
                self.provider_label, self.model_label
            )),
            _ => {}
        }

        self.overlay = Some(
            Overlay::table("session usage", &["metric", "value"], &[60, 40], rows)
                .with_footer(footer),
        );
    }

    /// `/queue` — show what is waiting; `clear` empties it, `drop` removes the
    /// most recent entry.
    fn run_queue_command(&mut self, args: &str) -> Option<Action> {
        match args.trim() {
            "" => {
                if self.queued.is_empty() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "nothing queued"));
                    return None;
                }
                let listed: Vec<String> = self
                    .queued
                    .iter()
                    .enumerate()
                    .map(|(i, q)| format!("{}. {q}", i + 1))
                    .collect();
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("queued ({}):\n{}", self.queued.len(), listed.join("\n")),
                ));
                None
            }
            "clear" => {
                let count = self.queued.len();
                self.queued.clear();
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    match count {
                        0 => "nothing queued".to_string(),
                        1 => "dropped the queued message".to_string(),
                        n => format!("dropped {n} queued messages"),
                    },
                ));
                None
            }
            "drop" => {
                match self.queued.pop_back() {
                    // Echoed back so "which one did I just lose" is answered
                    // without having to remember.
                    Some(text) => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, format!("dropped: {text}"))),
                    None => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, "nothing queued")),
                }
                None
            }
            other => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("unknown /queue subcommand: {other} — try /queue, /queue clear, or /queue drop"),
                ));
                None
            }
        }
    }

    /// `Ctrl+L` — open the diagnostics panel, or close it if it is already up.
    pub(super) fn toggle_log_panel(&mut self) {
        if self
            .overlay
            .as_ref()
            .is_some_and(|o| o.title == LOG_PANEL_TITLE)
        {
            self.overlay = None;
            return;
        }

        let lines: Vec<String> = self
            .logs
            .snapshot()
            .into_iter()
            .map(|l| format!("{:<5} {} — {}", l.level.label(), l.target, l.message))
            .collect();
        let empty = lines.is_empty();
        let body = if empty {
            vec!["nothing logged yet".to_string()]
        } else {
            lines
        };
        // Opened at the bottom: the interesting line in a log is the last one.
        let mut overlay = Overlay::lines(LOG_PANEL_TITLE, body).with_footer(vec![
            "Esc closes  ·  up/down and PgUp/PgDn scroll".to_string(),
        ]);
        overlay.scroll = u16::MAX;
        self.overlay = Some(overlay);
    }

    fn run_plan_command(&mut self, args: &str) -> Option<Action> {
        match args {
            "" => {
                let status = if self.plan_gated {
                    "awaiting approval — run /plan approve or /plan reject"
                } else {
                    "no plan pending"
                };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("plan status: {status}"),
                ));
                None
            }
            "approve" => {
                if !self.plan_gated && !self.modal.is_plan() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no plan pending to approve",
                    ));
                    return None;
                }
                self.modal = Modal::None;
                self.plan_gated = false;
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Building;
                self.in_flight_text = None;
                self.metrics.begin_turn();
                self.request_count += 1;
                self.lines
                    .push(ChatLine::new(ChatRole::System, "plan approved — building…"));
                Some(Action::ApprovePlan)
            }
            "reject" => {
                if !self.plan_gated && !self.modal.is_plan() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "no plan pending to reject"));
                    return None;
                }
                self.modal = Modal::None;
                self.plan_gated = false;
                self.lines
                    .push(ChatLine::new(ChatRole::System, "plan rejected"));
                Some(Action::RejectPlan)
            }
            description => {
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "still working on the previous request — wait for it to finish first",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::User,
                    format!("[plan] {description}"),
                ));
                self.waiting_on_assistant = true;
                self.plan_turn_active = true;
                self.phase = AgentPhase::Planning;
                self.in_flight_text = None;
                self.metrics.begin_turn();
                self.request_count += 1;
                Some(Action::StartPlan(description.to_string()))
            }
        }
    }

    fn run_goal_command(&mut self, args: &str) -> Option<Action> {
        match args {
            "" => {
                match &self.goal {
                    Some(goal) => self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!("current goal: {goal}"),
                    )),
                    None => self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no goal set — /goal <description> to set one, /goal clear to remove",
                    )),
                }
                None
            }
            "clear" => {
                if self.goal.is_none() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "no goal set"));
                    return None;
                }
                Some(Action::SetGoal(None))
            }
            description => Some(Action::SetGoal(Some(description.to_string()))),
        }
    }

    fn run_loop_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() {
            let status = match (self.loop_active, self.loop_progress) {
                (true, Some((i, m))) => format!("loop running — iteration {i}/{m} (Esc to cancel)"),
                (true, None) => "loop starting…".to_string(),
                (false, _) => "no loop running — /loop [<N>] <task>|goal to start one".to_string(),
            };
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("loop status: {status}"),
            ));
            return None;
        }

        if self.waiting_on_assistant {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "still working on the previous request — wait for it to finish first",
            ));
            return None;
        }

        let mut tokens = args.splitn(2, char::is_whitespace);
        let first = tokens.next().unwrap_or("");
        let (max_iterations, rest) = match first.parse::<u32>() {
            Ok(n) => (Some(n), tokens.next().unwrap_or("").trim()),
            Err(_) => (None, args),
        };

        if max_iterations == Some(0) {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "iteration count must be at least 1",
            ));
            return None;
        }

        let prompt = if rest.eq_ignore_ascii_case("goal") {
            match &self.goal {
                Some(goal) => goal.clone(),
                None => {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no goal set — /goal <description> first, or give /loop an explicit task",
                    ));
                    return None;
                }
            }
        } else if rest.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /loop [<N>] <task>|goal",
            ));
            return None;
        } else {
            rest.to_string()
        };

        self.lines
            .push(ChatLine::new(ChatRole::User, format!("[loop] {prompt}")));
        self.waiting_on_assistant = true;
        self.loop_active = true;
        self.loop_progress = None;
        self.phase = AgentPhase::Looping;
        self.in_flight_text = None;
        self.metrics.begin_turn();
        self.request_count += 1;
        Some(Action::StartLoop {
            prompt,
            max_iterations,
        })
    }
}
