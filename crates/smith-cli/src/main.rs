mod orchestrator;
mod prompts;
mod resources;
mod setup;

use clap::{Parser, Subcommand};
use smith_core::{Action, AgentEvent, ContentBlock, Message, PermissionAsk, QuestionAsk};
use smith_store::{Config, SessionStore};
use smith_tui::{ChatLine, ChatRole, IdleHint, TuiConfig};
use tokio::sync::mpsc;

use orchestrator::{run_orchestrator, Persistence, ProviderKind};

const GENERIC_TIP: &str = "run `smith setup` to add or change your provider or model";

#[derive(Debug, Parser)]
#[command(name = "smith", about = "A terminal AI coding agent")]
struct Cli {
    /// Which LLM provider to talk to (overrides the saved config).
    #[arg(long, value_enum)]
    provider: Option<ProviderKind>,

    /// Override the provider's default model.
    #[arg(long)]
    model: Option<String>,

    /// Resume a prior session by id (see the idle screen's "Continue" hint).
    #[arg(long)]
    resume: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Interactively configure a provider, API key, and model.
    Setup {
        #[command(subcommand)]
        resource: Option<SetupResource>,
    },
}

#[derive(Debug, Subcommand)]
enum SetupResource {
    /// Jump straight to picking a model for the already-configured provider.
    Model,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    if let Some(Commands::Setup { resource }) = cli.command {
        let jump_to_model = matches!(resource, Some(SetupResource::Model));
        return setup::run(jump_to_model).await;
    }

    let config = Config::load().unwrap_or_default();
    let provider_kind = cli
        .provider
        .or_else(|| {
            config
                .general
                .provider
                .as_deref()
                .and_then(ProviderKind::from_config_str)
        })
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

    let cwd = std::env::current_dir().unwrap_or_default();
    let session_store = SessionStore::open(&cwd).ok();

    let (session_id, initial_messages, idle_hint, initial_goal) =
        resolve_session(session_store.as_ref(), cli.resume.as_deref());

    let cwd_display = display_path(&cwd);
    let git_branch = detect_git_branch(&cwd);
    let initial_lines = messages_to_chat_lines(&initial_messages);

    let tui_config = TuiConfig {
        banner: smith_tui::banner::banner(),
        provider_label: provider_kind.label().to_string(),
        model_label: model.clone(),
        cwd_display,
        git_branch,
        idle_hint,
        initial_lines,
        permission_policy,
        goal: initial_goal.clone(),
        tasks: last_write_tasks_call(&initial_messages),
    };

    let (action_tx, action_rx) = mpsc::unbounded_channel::<Action>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel::<PermissionAsk>();
    let (question_tx, question_rx) = mpsc::unbounded_channel::<QuestionAsk>();

    let persistence = session_store.map(|store| Persistence {
        store,
        session_id,
        provider: provider_kind.label().to_string(),
        model: model.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        persisted: initial_messages.len(),
    });

    let resource_poller = if provider_kind == ProviderKind::Ollama {
        Some(tokio::spawn(resources::poll(
            event_tx.clone(),
            model.clone(),
        )))
    } else {
        None
    };

    let orchestrator = tokio::spawn(run_orchestrator(
        provider_kind,
        model,
        config,
        initial_messages,
        persistence,
        permission_policy,
        initial_goal,
        action_rx,
        event_tx,
        permission_tx,
        question_tx,
    ));

    smith_tui::run(tui_config, action_tx, event_rx, permission_rx, question_rx).await?;
    orchestrator.abort();
    if let Some(poller) = resource_poller {
        poller.abort();
    }

    Ok(())
}

/// Figures out what the idle screen should say about prior history and, for
/// `--resume <id>`, which session to reuse and preload. A fresh start does
/// NOT create a session row yet — that happens lazily on the first message,
/// so quitting from the idle screen without chatting doesn't leave an empty
/// session behind that would corrupt the next launch's "Continue" hint.
/// The goal (like the rest of a session's state) only ever comes back when
/// actually resuming that same session — a fresh start never inherits it
/// from whatever session last touched this project.
fn resolve_session(
    store: Option<&SessionStore>,
    resume: Option<&str>,
) -> (Option<String>, Vec<Message>, IdleHint, Option<String>) {
    let Some(store) = store else {
        return (
            None,
            Vec::new(),
            IdleHint::Tip(GENERIC_TIP.to_string()),
            None,
        );
    };

    if let Some(id) = resume {
        // `--resume` is an explicit instruction, so failing it must be loud.
        // Falling through to a blank session looked identical to a successful
        // resume of an empty conversation — the user would keep working,
        // believing their history was loaded, and only notice much later.
        match store.session_exists(id) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("smith: no session {id} in this project.");
                if let Ok(Some(latest)) = store.latest_session() {
                    eprintln!("smith: the most recent one here is {}.", latest.id);
                }
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("smith: could not read the session store: {e}");
                std::process::exit(1);
            }
        }

        let messages = match store.load_messages(id) {
            Ok(messages) => messages,
            Err(e) => {
                eprintln!("smith: could not resume session {id}: {e}");
                std::process::exit(1);
            }
        };
        let goal = store.load_goal(id).ok().flatten();
        return (
            Some(id.to_string()),
            messages,
            IdleHint::Tip(GENERIC_TIP.to_string()),
            goal,
        );
    }

    let idle_hint = match store.latest_session() {
        Ok(Some(summary)) => IdleHint::ContinueSession {
            title: summary.title,
            resume_cmd: format!("smith --resume {}", summary.id),
        },
        _ => IdleHint::Tip(GENERIC_TIP.to_string()),
    };

    (None, Vec::new(), idle_hint, None)
}

fn messages_to_chat_lines(messages: &[Message]) -> Vec<ChatLine> {
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

fn display_path(path: &std::path::Path) -> String {
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

fn detect_git_branch(cwd: &std::path::Path) -> Option<String> {
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
