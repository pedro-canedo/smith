//! Resolving what a run starts from: config, session, channels, first-run.

use crate::frontend::tui_theme;
use crate::subcommands::export_provisioned_browser;

use smith_config::Config;
use smith_core::{Action, AgentEvent, ContentBlock, Message, PermissionAsk, QuestionAsk};
use smith_store::SessionStore;
use smith_tui::{ChatLine, ChatRole, IdleHint, Theme};
use tokio::sync::mpsc;

use orchestrator::{OrchestratorChannels, Persistence, ProviderKind};

use super::*;

/// Env vars that count as "a provider is configured" even with an empty
/// config file, paired with the provider each one names. Exporting one is a
/// deliberate act; a CI box that did it must never be dropped into an
/// interactive menu, and must not be told about a provider it never named.
///
/// Order is the tie-break when several are exported. Anthropic leads because
/// it was the historical default, so a box that already worked keeps working.
pub(crate) const PROVIDER_KEY_ENVS: [(&str, ProviderKind); 4] = [
    ("ANTHROPIC_API_KEY", ProviderKind::Anthropic),
    ("OPENAI_API_KEY", ProviderKind::Openai),
    ("OPENROUTER_API_KEY", ProviderKind::Openrouter),
    ("NINEROUTER_API_KEY", ProviderKind::NineRouter),
];

pub(crate) fn provider_from_exported_key() -> Option<ProviderKind> {
    PROVIDER_KEY_ENVS
        .iter()
        .find(|(var, _)| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()))
        .map(|(_, kind)| *kind)
}

/// True when this run has nothing to talk to and nobody has said otherwise.
///
/// `--provider` counts, because it names one outright. So does any saved
/// `[general] provider`, and so does an exported key. Everything else is a
/// fresh install.
pub(crate) fn needs_first_run_setup(cli: &Cli) -> bool {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = Config::load_layered(&cwd).unwrap_or_default();
    first_run_needed(
        cli.provider,
        provider_from_exported_key(),
        config.general.provider.as_deref(),
    )
}

/// The decision itself, without the environment. Split out because the three
/// inputs are a process-global env var, the working directory and a parsed
/// config — none of which a test can vary in parallel with another test.
pub(crate) fn first_run_needed(
    from_flag: Option<ProviderKind>,
    from_env: Option<ProviderKind>,
    from_config: Option<&str>,
) -> bool {
    from_flag.is_none()
        && from_env.is_none()
        // A config naming a provider smith does not understand is not a
        // configured provider. Treating it as one would send the user into
        // the same dead end this branch exists to prevent, one layer deeper.
        && from_config.and_then(ProviderKind::from_config_str).is_none()
}

/// Rust sets `SIGPIPE` to `SIG_IGN` at startup, which turns `smith sessions
/// list | head` into a panic — "failed printing to stdout: Broken pipe" —
/// instead of the silent exit every other Unix tool gives you. A CLI that
/// documents piping its output has to behave like one.
///
/// Restoring the default handler is the standard fix and has to happen before
/// anything writes. There is no stable safe API for it.
#[cfg(unix)]
pub(crate) fn restore_default_sigpipe() {
    // SAFETY: setting a signal disposition to the OS default, before any
    // thread has been spawned or any output written.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
pub(crate) fn restore_default_sigpipe() {}

/// State both frontends need, resolved identically for each so a headless run
/// and an interactive one disagree about nothing except the frontend.
pub(crate) struct Startup {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) config: Config,
    pub(crate) provider_kind: ProviderKind,
    pub(crate) model: String,
    pub(crate) permission_policy: smith_core::PermissionPolicy,
    pub(crate) session_store: Option<SessionStore>,
    pub(crate) session_id: Option<String>,
    pub(crate) initial_messages: Vec<Message>,
    pub(crate) idle_hint: IdleHint,
    pub(crate) initial_goal: Option<String>,
    /// The output style this session runs under. Resolved here rather than in
    /// the orchestrator because `--persona` is a CLI flag, and both frontends
    /// have to agree about it.
    pub(crate) persona: Option<smith_config::Persona>,
    /// Custom slash commands. Discovered here so a broken file is reported
    /// once at startup, like a broken subagent definition.
    pub(crate) commands: smith_config::CommandSet,
    /// The resolved palette. Only the TUI paints with it, but it is resolved
    /// here, for both frontends, so that a bad `--theme` or a bad hex value
    /// in `[theme.colors]` is reported by every invocation — including the
    /// headless one, which is where a CI config is usually first typed.
    pub(crate) theme: Theme,
    /// Resolved key bindings. Like the theme, resolved for both frontends so
    /// a typo in `[keys]` is reported even by a headless run that will never
    /// press a key.
    pub(crate) keys: smith_tui::KeyMap,
}

impl Startup {
    pub(crate) fn resolve(cli: &Cli) -> Result<Self, String> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let config = Config::load_layered(&cwd).unwrap_or_default();
        export_provisioned_browser(&config);
        let provider_kind = cli
            .provider
            .or_else(|| {
                config
                    .general
                    .provider
                    .as_deref()
                    .and_then(ProviderKind::from_config_str)
            })
            // An exported key is the only remaining statement of intent, and
            // it says which provider. Without this, a box with just
            // `OPENAI_API_KEY` set reached the old Anthropic default and was
            // told it had no Anthropic key — naming a provider the user never
            // mentioned, about a key they never had to have.
            .or_else(provider_from_exported_key)
            .unwrap_or(ProviderKind::Anthropic);
        let model = cli
            .model
            .clone()
            .or_else(|| config.general.model.clone())
            .unwrap_or_else(|| provider_kind.default_model().to_string());
        let permission_policy = config
            .general
            .permission_policy
            .as_deref()
            .and_then(smith_core::PermissionPolicy::parse)
            .unwrap_or_default();

        // A named persona that is missing is a usage error — the user typed a
        // name and got a different agent than they asked for, silently. The
        // implicit `default` is absent for almost everyone, so its absence is
        // not reported at all.
        let explicit = cli.persona.is_some();
        let persona = smith_config::extend::persona::load_in(
            None,
            &cwd,
            cli.persona
                .as_deref()
                .unwrap_or(smith_config::extend::persona::DEFAULT_PERSONA),
            explicit,
        )?;
        let commands = smith_config::CommandSet::discover(&cwd, &smith_tui::slash::builtin_names());
        let theme = tui_theme(cli.ascii, cli.theme.as_deref(), &config.theme)?;
        let keys = smith_tui::KeyMap::from_overrides(
            config.keys.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .map_err(|e| format!("smith: {e}"))?;

        // History moved out of `<project>/.smith/` and into `~/.smith/projects/`.
        // Adopting an existing database is announced rather than silent: a
        // session list that relocates looks exactly like one that was lost.
        match smith_config::adopt_legacy_session_db(&cwd) {
            Ok(Some(from)) => eprintln!(
                "smith: moved this project's history out of {} — it now lives under ~/.smith",
                from.display()
            ),
            Ok(None) => {}
            Err(e) => eprintln!("smith: could not move the old session database: {e}"),
        }
        let session_store = smith_config::project_store_dir(&cwd)
            .ok()
            .and_then(|dir| SessionStore::open(&dir).ok());
        let (session_id, initial_messages, idle_hint, initial_goal) =
            resolve_session(session_store.as_ref(), cli.resume.as_deref(), cli.continue_)?;

        Ok(Self {
            cwd,
            config,
            provider_kind,
            model,
            permission_policy,
            session_store,
            session_id,
            initial_messages,
            idle_hint,
            initial_goal,
            persona,
            commands,
            theme,
            keys,
        })
    }

    pub(crate) fn persistence(&mut self) -> Option<Persistence> {
        let store = self.session_store.take()?;
        Some(Persistence {
            store,
            session_id: self.session_id.clone(),
            provider: self.provider_kind.label().to_string(),
            model: self.model.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            persisted: self.initial_messages.len(),
        })
    }
}

pub(crate) fn channels() -> (
    mpsc::UnboundedSender<Action>,
    OrchestratorChannels,
    mpsc::UnboundedReceiver<AgentEvent>,
    mpsc::UnboundedReceiver<PermissionAsk>,
    mpsc::UnboundedReceiver<QuestionAsk>,
) {
    let (action_tx, action_rx) = mpsc::unbounded_channel::<Action>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel::<PermissionAsk>();
    let (question_tx, question_rx) = mpsc::unbounded_channel::<QuestionAsk>();
    (
        action_tx,
        OrchestratorChannels {
            action_rx,
            event_tx,
            permission_tx,
            question_tx,
        },
        event_rx,
        permission_rx,
        question_rx,
    )
}

/// Figures out what the idle screen should say about prior history and, for
/// `--resume <id>`, which session to reuse and preload. A fresh start does
/// NOT create a session row yet — that happens lazily on the first message,
/// so quitting from the idle screen without chatting doesn't leave an empty
/// session behind that would corrupt the next launch's "Continue" hint.
/// The goal (like the rest of a session's state) only ever comes back when
/// actually resuming that same session — a fresh start never inherits it
/// from whatever session last touched this project.
///
/// Errors come back as a ready-to-print message rather than being exited on
/// in place: the two frontends owe the caller different exit codes, and a
/// `process::exit` buried here can't honour either.
#[allow(clippy::type_complexity)]
pub(crate) fn resolve_session(
    store: Option<&SessionStore>,
    resume: Option<&str>,
    continue_latest: bool,
) -> Result<(Option<String>, Vec<Message>, IdleHint, Option<String>), String> {
    let Some(store) = store else {
        return Ok((
            None,
            Vec::new(),
            IdleHint::NewSession {
                title: new_session_title(),
            },
            None,
        ));
    };

    // Resolved to a concrete id up front so `--continue` and `--resume` share
    // one code path — and one set of error messages — from here on.
    let resolved: Option<String> = match (resume, continue_latest) {
        (Some(id), _) => Some(id.to_string()),
        (None, true) => match store.latest_session() {
            Ok(Some(summary)) => Some(summary.id),
            Ok(None) => {
                return Err("no saved sessions in this project yet".to_string());
            }
            Err(e) => return Err(format!("could not read the session store: {e}")),
        },
        (None, false) => None,
    };

    if let Some(id) = resolved.as_deref() {
        // `--resume` is an explicit instruction, so failing it must be loud.
        // Falling through to a blank session looked identical to a successful
        // resume of an empty conversation — the user would keep working,
        // believing their history was loaded, and only notice much later.
        match store.session_exists(id) {
            Ok(true) => {}
            Ok(false) => {
                let mut message = format!("smith: no session {id} in this project.");
                if let Ok(Some(latest)) = store.latest_session() {
                    message.push_str(&format!(
                        "\nsmith: the most recent one here is {}.",
                        latest.id
                    ));
                }
                return Err(message);
            }
            Err(e) => return Err(format!("smith: could not read the session store: {e}")),
        }

        let messages = store
            .load_messages(id)
            .map_err(|e| format!("smith: could not resume session {id}: {e}"))?;
        let goal = store.load_goal(id).ok().flatten();
        return Ok((
            Some(id.to_string()),
            messages,
            IdleHint::Tip(GENERIC_TIP.to_string()),
            goal,
        ));
    }

    let idle_hint = match store.latest_session() {
        Ok(Some(summary)) => IdleHint::ContinueSession {
            title: format!(
                "New session - {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            ),
            resume_cmd: format!("smith --resume {}", summary.id),
        },
        _ => IdleHint::NewSession {
            title: new_session_title(),
        },
    };

    Ok((None, Vec::new(), idle_hint, None))
}

pub(crate) fn new_session_title() -> String {
    format!(
        "New session - {}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    )
}

pub(crate) fn messages_to_chat_lines(messages: &[Message]) -> Vec<ChatLine> {
    let mut lines = Vec::new();
    for message in messages {
        match message.role {
            smith_core::Role::User => {
                let text = message.text();
                if !text.is_empty() {
                    lines.push(ChatLine::new(ChatRole::User, text));
                }
            }
            smith_core::Role::Assistant => {
                let text = message.text();
                if !text.is_empty() {
                    lines.push(ChatLine::new(ChatRole::Assistant, text));
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { name, .. } = block {
                        // Bookkeeping-only tool — surfaced via the Tasks
                        // sidebar (see `last_write_tasks_call`), not the
                        // transcript.
                        if name != "write_tasks" {
                            lines.push(ChatLine::new(
                                ChatRole::System,
                                format!("used tool: {name}"),
                            ));
                        }
                    }
                }
            }
        }
    }
    lines
}

/// The user's own prompts from a resumed session, most recent first, so the
/// Up key reaches them the way it would in a shell.
///
/// Only messages the *user* typed: a `Role::User` message can also be a
/// carrier for tool results, and recalling `[{"type":"tool_result",...}]` into
/// the prompt box would be nonsense. Text blocks are the only ones a user
/// could have produced by typing.
pub(crate) fn prompt_history(messages: &[Message]) -> Vec<String> {
    let mut history: Vec<String> = Vec::new();
    for message in messages {
        if message.role != smith_core::Role::User {
            continue;
        }
        let text: String = message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            history.push(text);
        }
    }
    history.reverse();
    history.truncate(smith_tui::app::HISTORY_LIMIT);
    history
}

/// Rebuilds the checklist from the last `write_tasks` call in a resumed
/// session's history, so `--resume` doesn't start with a blank Tasks panel.
/// `pub(crate)` — also used by `orchestrator::run_orchestrator` to seed the
/// agent's own task state on startup/resume.
pub(crate) fn last_write_tasks_call(messages: &[Message]) -> Vec<smith_core::Task> {
    messages
        .iter()
        .rev()
        .flat_map(|m| m.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } if name == "write_tasks" => {
                smith_core::parse_tasks(input).ok()
            }
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) fn display_path(path: &std::path::Path) -> String {
    let path_str = path.to_string_lossy();
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path_str.into_owned()
}

pub(crate) fn detect_git_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}
