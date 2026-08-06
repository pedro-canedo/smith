mod doctor;
mod headless;
mod logging;
mod node_runtime;
mod orchestrator;
mod prompts;
mod resources;
mod runtime;
mod setup;
mod subagents;
mod update;

use std::collections::BTreeSet;
use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use smith_config::Config;
use smith_core::{Action, AgentEvent, ContentBlock, Message, PermissionAsk, QuestionAsk};
use smith_store::SessionStore;
use smith_tui::{ChatLine, ChatRole, IdleHint, Theme, TuiConfig};
use tokio::sync::mpsc;

use headless::{HeadlessOptions, OutputFormat, EXIT_OK, EXIT_TURN_FAILED, EXIT_USAGE};
use orchestrator::{
    run_orchestrator, OrchestratorChannels, OrchestratorOptions, Persistence, ProviderKind,
};

const GENERIC_TIP: &str = "run `smith setup` to add or change your provider or model";

#[derive(Debug, Parser)]
#[command(name = "smith", version, about = "A terminal AI coding agent")]
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

    /// Resume the most recent session in this project. The idle screen used
    /// to print the id for you to copy — this is the same thing without the
    /// copying.
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_: bool,

    /// Run one turn non-interactively with this prompt and exit. Anything
    /// piped to stdin is appended to it as context.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// How a non-interactive run reports itself. Implies --print.
    #[arg(long, value_enum, value_name = "FORMAT")]
    output_format: Option<OutputFormat>,

    /// Force ASCII glyphs in the terminal UI.
    #[arg(long)]
    ascii: bool,

    /// Palette for the terminal UI: `dark` (default), `light` or
    /// `high_contrast`. Overrides `[theme] name` in the config. An unknown
    /// name is an error — see `smith_tui::theme`.
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,

    /// Screen-reader friendly output: no TUI, no chrome, no colour escapes.
    #[arg(long)]
    plain: bool,

    /// Debug-only: initialize the TUI terminal and then panic, for PTY tests.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true)]
    panic_now: bool,

    /// Cap the tool-call rounds one turn may take before it stops itself.
    #[arg(long, value_name = "N")]
    max_turns: Option<u32>,

    /// Tools a non-interactive run may use beyond the read-only ones, e.g.
    /// `--allowed-tools write_file,edit_file,run_bash`. Anything not listed is
    /// denied — there is no prompt to answer without a terminal.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    allowed_tools: Vec<String>,

    /// Work in this directory instead of the current one.
    #[arg(long, value_name = "DIR")]
    cwd: Option<std::path::PathBuf>,

    /// Output style to run under: a file in `.smith/personas/` or
    /// `~/.smith/personas/`. Defaults to `default` if such a file exists;
    /// `--persona none` disables it. Read once at startup, deliberately —
    /// see `prompts::system_prompt_with`.
    #[arg(long, value_name = "NAME")]
    persona: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

impl Cli {
    /// Whether this invocation is non-interactive.
    ///
    /// `-p` is the explicit request. The stdout check is the implicit one and
    /// matters just as much: a TUI writing escape sequences into a pipe or a
    /// CI log produces garbage that also never exits, because there is no
    /// terminal to send it the keystroke that would quit.
    fn is_headless(&self, stdout_is_tty: bool) -> bool {
        self.print.is_some()
            || self.output_format.is_some()
            || self.plain
            || is_dumb_terminal()
            || !stdout_is_tty
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Interactively configure a provider, API key, and model.
    Setup {
        #[command(subcommand)]
        resource: Option<SetupResource>,
    },
    /// Append a standing instruction to this project's SMITH.md, which is
    /// folded into the system prompt on every request from now on.
    Remember {
        /// The note. Multiple words need no quoting.
        #[arg(required = true, num_args = 1.., value_name = "NOTE")]
        note: Vec<String>,

        /// Write to `~/.smith/SMITH.md` instead — every project, not just
        /// this one.
        #[arg(long)]
        global: bool,
    },
    /// Inspect and manage this project's saved conversations.
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Check config, credentials, connectivity, runtimes, directory
    /// permissions and MCP servers. Exits non-zero if anything FAILs, so it
    /// can gate a CI job.
    Doctor,
    /// Check for and install the latest published Smith release.
    Update,
}

#[derive(Debug, Subcommand)]
enum SessionAction {
    /// Most recently touched first.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Delete a session and everything recorded against it.
    Delete { id: String },
    /// Branch a session into a new one, optionally truncating it first.
    Fork {
        id: String,
        /// Copy messages up to and including this sequence number. Omit to
        /// copy the whole conversation; `sessions list` shows the range.
        #[arg(long)]
        through: Option<i64>,
    },
    /// Write a session to stdout as markdown or JSON, for sharing or
    /// archiving outside smith.
    Export {
        id: String,
        #[arg(long, value_enum, default_value_t = ExportFormat::Markdown)]
        format: ExportFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ExportFormat {
    Markdown,
    Json,
}

#[derive(Debug, Subcommand)]
enum SetupResource {
    /// Jump straight to picking a model for the already-configured provider.
    Model,
}

#[tokio::main]
async fn main() -> ExitCode {
    restore_default_sigpipe();
    // First, so the crates below can be diagnosed while they start up. The
    // file sink goes under `~/.smith/logs/` rather than the project's
    // `.smith/`: a provider timeout or a broken MCP command is a property of
    // this machine's setup, not of the repository you happened to run in.
    let logs = logging::install(smith_config::config_dir().ok().map(|d| d.join("logs")));
    let mut cli = Cli::parse();
    let command = cli.command.take();

    if let Some(Commands::Setup { resource }) = &command {
        let jump_to_model = matches!(resource, Some(SetupResource::Model));
        return match setup::run(jump_to_model).await {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("smith: {e}");
                ExitCode::from(EXIT_USAGE)
            }
        };
    }

    if matches!(&command, Some(Commands::Update)) {
        return update::run().await;
    }

    // Before anything reads the working directory — the config layering, the
    // session store and the tools' sandbox root all derive from it, and they
    // must all agree on which directory this run is about.
    if let Some(dir) = &cli.cwd {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("smith: cannot use --cwd {}: {e}", dir.display());
            return ExitCode::from(EXIT_USAGE);
        }
    }

    #[cfg(debug_assertions)]
    if cli.panic_now {
        return match smith_tui::panic_after_terminal_init_for_test() {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("smith: {e}");
                ExitCode::from(EXIT_TURN_FAILED)
            }
        };
    }

    // After `--cwd`, unlike `setup`: which SMITH.md this writes to is decided
    // by the project directory, so it has to see the same one a session would.
    if let Some(Commands::Remember { note, global }) = &command {
        return run_remember(&note.join(" "), *global);
    }

    if let Some(Commands::Sessions { action }) = &command {
        return run_sessions(action);
    }

    // After `--cwd`, because the project config layer, `.smith/` and the
    // session store it inspects all hang off the working directory — doctor
    // has to be looking at the same project a real run would.
    if let Some(Commands::Doctor) = &command {
        return ExitCode::from(doctor::run().await);
    }

    let headless = cli.is_headless(std::io::stdout().is_terminal());

    // A first run used to guess Anthropic and then fail on a key the user was
    // never asked for, blaming them for a choice the code made. There is no
    // honest guess to make here, so it stops guessing: with a terminal, the
    // wizard opens; without one, the error names every way out instead of the
    // one provider that happened to be first in an enum.
    if needs_first_run_setup(&cli) {
        if headless {
            eprintln!("smith: no provider configured yet. Pick one of:");
            eprintln!("  smith setup                       — interactive, saves to ~/.smith");
            eprintln!("  smith --provider ollama --model <name> -p '…'   — one run, no config");
            eprintln!("  export ANTHROPIC_API_KEY=…        — or OPENAI_/OPENROUTER_/NINEROUTER_");
            return ExitCode::from(EXIT_USAGE);
        }
        println!("smith: no provider configured yet — starting setup.\n");
        if let Err(e) = setup::run(false).await {
            eprintln!("smith: setup failed: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
        // Re-read rather than trust: the wizard may have been Esc'd out of
        // with nothing chosen, and starting a session that cannot answer is
        // the failure this whole branch exists to avoid.
        if needs_first_run_setup(&cli) {
            eprintln!("smith: still no provider configured — {GENERIC_TIP}");
            return ExitCode::from(EXIT_USAGE);
        }
    }

    if !headless {
        if std::env::var("SMITH_AUTO_UPDATE").as_deref() == Ok("1") {
            update::auto_update().await;
        } else {
            update::startup_notice().await;
        }
    }

    if headless {
        ExitCode::from(run_headless(cli).await)
    } else {
        run_tui(cli, logs).await
    }
}

/// Env vars that count as "a provider is configured" even with an empty
/// config file, paired with the provider each one names. Exporting one is a
/// deliberate act; a CI box that did it must never be dropped into an
/// interactive menu, and must not be told about a provider it never named.
///
/// Order is the tie-break when several are exported. Anthropic leads because
/// it was the historical default, so a box that already worked keeps working.
const PROVIDER_KEY_ENVS: [(&str, ProviderKind); 4] = [
    ("ANTHROPIC_API_KEY", ProviderKind::Anthropic),
    ("OPENAI_API_KEY", ProviderKind::Openai),
    ("OPENROUTER_API_KEY", ProviderKind::Openrouter),
    ("NINEROUTER_API_KEY", ProviderKind::NineRouter),
];

fn provider_from_exported_key() -> Option<ProviderKind> {
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
fn needs_first_run_setup(cli: &Cli) -> bool {
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
fn first_run_needed(
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
fn restore_default_sigpipe() {
    // SAFETY: setting a signal disposition to the OS default, before any
    // thread has been spawned or any output written.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_default_sigpipe() {}

/// `smith sessions …` — list, delete, fork or export saved conversations.
///
/// Read-only by default and destructive only on an explicit `delete`; every
/// subcommand names the session it acted on, since ids are opaque and a
/// silent success on the wrong one is unrecoverable.
fn run_sessions(action: &SessionAction) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut store = match SessionStore::open(&cwd) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("smith: could not open this project's session store: {e}");
            return ExitCode::from(2);
        }
    };

    let result = match action {
        SessionAction::List { limit } => list_sessions(&store, *limit),
        SessionAction::Delete { id } => match store.delete_session(id) {
            Ok(true) => {
                println!("deleted {id}");
                Ok(())
            }
            Ok(false) => Err(format!("no session {id} in this project")),
            Err(e) => Err(e.to_string()),
        },
        SessionAction::Fork { id, through } => match store.fork_session(id, *through) {
            Ok(new_id) => {
                println!("forked {id} -> {new_id}");
                println!("resume it with: smith --resume {new_id}");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        SessionAction::Export { id, format } => export_session(&store, id, *format),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::from(2)
        }
    }
}

fn list_sessions(store: &SessionStore, limit: u32) -> Result<(), String> {
    let sessions = store
        .list_sessions(Some(limit))
        .map_err(|e| e.to_string())?;
    if sessions.is_empty() {
        println!("no saved sessions in this project");
        return Ok(());
    }
    for summary in sessions {
        let turns = store.turn_totals(&summary.id).map(|t| t.turns).unwrap_or(0);
        let last = store.last_seq(&summary.id).ok().flatten();
        // The seq range is printed because `sessions fork --through` is
        // meaningless without knowing what the numbers can be.
        let range = match last {
            Some(seq) => format!("seq 0..={seq}"),
            None => "empty".to_string(),
        };
        println!(
            "{}  {}  ({turns} turns, {range})",
            summary.id, summary.title
        );
    }
    Ok(())
}

fn export_session(store: &SessionStore, id: &str, format: ExportFormat) -> Result<(), String> {
    if !store.session_exists(id).map_err(|e| e.to_string())? {
        return Err(format!("no session {id} in this project"));
    }
    let messages = store.load_messages(id).map_err(|e| e.to_string())?;

    match format {
        ExportFormat::Json => {
            let json = serde_json::to_string_pretty(&messages).map_err(|e| e.to_string())?;
            println!("{json}");
        }
        ExportFormat::Markdown => {
            println!("# smith session {id}\n");
            for message in &messages {
                let text = message.text();
                if text.trim().is_empty() {
                    // Tool-call rounds carry no prose; a heading with nothing
                    // under it is noise in a document meant to be read.
                    continue;
                }
                let who = match message.role {
                    smith_core::Role::User => "user",
                    smith_core::Role::Assistant => "assistant",
                };
                println!("## {who}\n\n{text}\n");
            }
        }
    }
    Ok(())
}

/// `smith remember <note>` — appends a standing instruction to a `SMITH.md`.
///
/// The project target is the *root* of the memory chain, not the working
/// directory: a note dropped into whatever subdirectory you happened to be in
/// would only apply while you stayed there, which is not what "remember this"
/// means. `--global` picks `~/.smith/SMITH.md` instead.
fn run_remember(note: &str, global: bool) -> ExitCode {
    let path = if global {
        smith_config::memory::global_memory_path()
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(smith_config::memory::memory_path(
            &smith_config::MemoryScope::discover(cwd).root,
        ))
    };

    let result = path.and_then(|path| {
        smith_config::memory::remember(&path, note)?;
        Ok(path)
    });

    match result {
        Ok(path) => {
            println!("remembered in {}", path.display());
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Hands the browser `smith setup` provisioned to `smith-tools`.
///
/// `smith_tools::chromium` resolves a browser from `SMITH_CHROMIUM_PATH`,
/// then `CHROME_PATH`, then `PATH` — it knows nothing about `smith-config`,
/// and `smith-cli` cannot reach into it to teach it. Exporting the variable is
/// the seam that already exists, so no change to that crate is needed for a
/// provisioned browser to be found. (Having `chromium.rs` probe
/// `~/.smith/runtime` itself would be tidier; see the note filed with this
/// change.)
///
/// A variable the user set themselves is never overwritten: an explicit
/// override has to keep winning, which is the whole contract of those two
/// variables.
fn export_provisioned_browser(config: &Config) {
    let Some(path) = browser_path_to_export(config, |v| std::env::var(v).ok()) else {
        return;
    };
    // Safe here in the way `set_var` needs to be: this runs during startup
    // resolution, before any tool, provider or TUI task exists, so nothing
    // else is reading the environment concurrently.
    std::env::set_var(runtime::BROWSER_PATH_ENV, path);
}

/// The decision `export_provisioned_browser` makes, without the side effect —
/// so the precedence is testable without mutating the process environment out
/// from under every other test in the binary.
fn browser_path_to_export(config: &Config, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let path = config
        .runtime
        .chromium_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    // A variable the user set themselves is never overwritten: an explicit
    // override has to keep winning, which is the whole contract of these two.
    for var in [
        runtime::BROWSER_PATH_ENV,
        runtime::BROWSER_PATH_ENV_FALLBACK,
    ] {
        if env(var).is_some_and(|v| !v.trim().is_empty()) {
            return None;
        }
    }
    Some(path.to_string())
}

/// State both frontends need, resolved identically for each so a headless run
/// and an interactive one disagree about nothing except the frontend.
struct Startup {
    cwd: std::path::PathBuf,
    config: Config,
    provider_kind: ProviderKind,
    model: String,
    permission_policy: smith_core::PermissionPolicy,
    session_store: Option<SessionStore>,
    session_id: Option<String>,
    initial_messages: Vec<Message>,
    idle_hint: IdleHint,
    initial_goal: Option<String>,
    /// The output style this session runs under. Resolved here rather than in
    /// the orchestrator because `--persona` is a CLI flag, and both frontends
    /// have to agree about it.
    persona: Option<smith_config::Persona>,
    /// Custom slash commands. Discovered here so a broken file is reported
    /// once at startup, like a broken subagent definition.
    commands: smith_config::CommandSet,
    /// The resolved palette. Only the TUI paints with it, but it is resolved
    /// here, for both frontends, so that a bad `--theme` or a bad hex value
    /// in `[theme.colors]` is reported by every invocation — including the
    /// headless one, which is where a CI config is usually first typed.
    theme: Theme,
    /// Resolved key bindings. Like the theme, resolved for both frontends so
    /// a typo in `[keys]` is reported even by a headless run that will never
    /// press a key.
    keys: smith_tui::KeyMap,
}

impl Startup {
    fn resolve(cli: &Cli) -> Result<Self, String> {
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

        let session_store = SessionStore::open(&cwd).ok();
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

    fn persistence(&mut self) -> Option<Persistence> {
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

fn channels() -> (
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

async fn run_tui(cli: Cli, logs: smith_tui::LogBuffer) -> ExitCode {
    if let Err(e) = color_eyre::install() {
        eprintln!("smith: {e}");
        return ExitCode::from(EXIT_TURN_FAILED);
    }

    let mut startup = match Startup::resolve(&cli) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let commands = std::mem::take(&mut startup.commands);
    // A command file that would not load is announced at startup. A `/deploy`
    // that silently does not exist is indistinguishable from one the user
    // mistyped, and the file it was shadowed by or refused for is the only
    // useful thing to say about it — same reasoning as the subagent loader's
    // problem lines.
    let mut initial_lines = messages_to_chat_lines(&startup.initial_messages);
    for problem in &commands.problems {
        initial_lines.push(ChatLine::new(
            ChatRole::System,
            format!("custom command {problem}"),
        ));
    }
    if let Some(persona) = &startup.persona {
        initial_lines.push(ChatLine::new(
            ChatRole::System,
            format!(
                "output style: {} ({})",
                persona.name,
                persona.source.display()
            ),
        ));
    }

    let tui_config = TuiConfig {
        banner: smith_tui::banner::banner(),
        provider_label: startup.provider_kind.label().to_string(),
        model_label: startup.model.clone(),
        cwd_display: display_path(&startup.cwd),
        git_branch: detect_git_branch(&startup.cwd),
        idle_hint: std::mem::replace(&mut startup.idle_hint, IdleHint::Tip(String::new())),
        initial_lines,
        permission_policy: startup.permission_policy,
        theme: startup.theme.clone(),
        keys: startup.keys.clone(),
        goal: startup.initial_goal.clone(),
        tasks: last_write_tasks_call(&startup.initial_messages),
        history: prompt_history(&startup.initial_messages),
        commands: smith_tui::slash::SlashRegistry::new(commands),
        logs,
    };

    let (action_tx, chans, event_rx, permission_rx, question_rx) = channels();
    let event_tx = chans.event_tx.clone();

    let mut opts = OrchestratorOptions::new(
        startup.provider_kind,
        startup.model.clone(),
        startup.config.clone(),
    );
    opts.persistence = startup.persistence();
    opts.permission_policy = startup.permission_policy;
    opts.initial_goal = startup.initial_goal.clone();
    opts.initial_messages = std::mem::take(&mut startup.initial_messages);
    opts.persona = startup.persona.take();
    if let Some(max_turns) = cli.max_turns {
        opts.limits.max_turns = max_turns;
    }

    let resource_poller = if startup.provider_kind == ProviderKind::Ollama {
        Some(tokio::spawn(resources::poll(
            event_tx,
            startup.model.clone(),
        )))
    } else {
        None
    };

    let orchestrator = tokio::spawn(run_orchestrator(opts, chans));

    let result = smith_tui::run(tui_config, action_tx, event_rx, permission_rx, question_rx).await;
    orchestrator.abort();
    if let Some(poller) = resource_poller {
        poller.abort();
    }

    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::from(EXIT_TURN_FAILED)
        }
    }
}

async fn run_headless(cli: Cli) -> u8 {
    let piped = read_piped_stdin();
    let prompt = match headless::compose_prompt(cli.print.as_deref(), piped.as_deref()) {
        Ok(prompt) => prompt,
        Err(message) => {
            eprintln!("smith: {message}");
            return EXIT_USAGE;
        }
    };

    let mut startup = match Startup::resolve(&cli) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };

    // Built here rather than inside the orchestrator so a missing API key is a
    // clean exit code and one line on stderr, instead of an `Error` event that
    // has to race the turn it prevented.
    let provider = match orchestrator::build_provider_stack(startup.provider_kind, &startup.config)
    {
        Ok(provider) => provider,
        Err(message) => {
            eprintln!("smith: {message}");
            return EXIT_USAGE;
        }
    };

    let (action_tx, chans, event_rx, permission_rx, question_rx) = channels();

    let mut opts = OrchestratorOptions::new(
        startup.provider_kind,
        startup.model.clone(),
        startup.config.clone(),
    );
    opts.provider = Some(provider);
    opts.persistence = startup.persistence();
    opts.initial_goal = startup.initial_goal.clone();
    opts.initial_messages = std::mem::take(&mut startup.initial_messages);
    // A persona applies headless too: it is how the run writes, and a CI job
    // that wants smith's default voice simply does not pass `--persona`. Its
    // invariant half is not up for negotiation either way — see
    // `prompts::PROMPT_INVARIANTS`.
    opts.persona = startup.persona.take();
    // Deliberately *not* `startup.permission_policy`. A saved `skip` (or
    // `session`) would auto-allow tools before they ever reach the permission
    // channel, which is the only place `--allowed-tools` can see them — a
    // config file written months ago would silently widen what a CI job may
    // do. Forcing `ask` makes the flag the single gate.
    opts.permission_policy = smith_core::PermissionPolicy::Ask;
    // …and this closes the two ways a call could still skip that gate. Both
    // exemptions exist to avoid interrupting a human: a write confined to the
    // scratch directory, and `task`, whose child can only read. Neither
    // argument survives without a human — the channel answers instantly from
    // a list — while both left a call running in a job that named no tools.
    opts.unattended = true;
    if let Some(max_turns) = cli.max_turns {
        opts.limits.max_turns = max_turns;
    }

    let orchestrator = tokio::spawn(run_orchestrator(opts, chans));

    let options = HeadlessOptions {
        prompt,
        format: cli.output_format.unwrap_or_default(),
        allowed_tools: cli.allowed_tools.iter().cloned().collect::<BTreeSet<_>>(),
        color: headless_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::io::stderr().is_terminal(),
            cli.plain,
        ),
        provider: startup.provider_kind.label().to_string(),
        model: startup.model.clone(),
    };

    let code = headless::run(
        &options,
        action_tx,
        event_rx,
        permission_rx,
        question_rx,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await;
    orchestrator.abort();
    code
}

/// The palette this run paints in.
///
/// Precedence is `--theme` > project `.smith/config.toml` > global
/// `~/.smith/config.toml` > the detected default, and the two config layers
/// have already been merged into `config` by `Config::load_layered`. Every
/// failure is a usage error rather than a fallback: a theme name or a hex
/// value that did not take effect is invisible, and a user who cannot see
/// that their config was ignored will file it as "the flag does nothing".
fn tui_theme(
    force_ascii: bool,
    flag: Option<&str>,
    config: &smith_config::ThemeSettings,
) -> Result<Theme, String> {
    let name = flag.or(config.name.as_deref());
    let theme = Theme::resolve(name, &config.colors).map_err(|e| format!("smith: {e}"))?;
    Ok(if force_ascii {
        theme.ascii_glyphs()
    } else {
        theme
    })
}

fn is_dumb_terminal() -> bool {
    term_is_dumb(std::env::var_os("TERM").as_deref())
}

fn term_is_dumb(term: Option<&std::ffi::OsStr>) -> bool {
    term.and_then(std::ffi::OsStr::to_str) == Some("dumb")
}

/// Whether the text format may use ANSI colour.
///
/// `NO_COLOR` (https://no-color.org) wins when set to anything non-empty — the
/// spec is explicit that an empty value does *not* count, so `NO_COLOR=` set
/// by an over-eager wrapper script doesn't silently strip colour. A
/// non-terminal stderr disables it too: the chrome lives on stderr, so that is
/// the stream whose capabilities decide, not stdout's.
fn color_enabled(no_color: Option<&std::ffi::OsStr>, stderr_is_tty: bool) -> bool {
    let disabled = no_color.is_some_and(|v| !v.is_empty());
    !disabled && stderr_is_tty
}

/// Whether a headless run styles its chrome.
///
/// Split out of the `HeadlessOptions` literal so `--plain`'s promise is a
/// testable function rather than an `&&` nobody can reach. It matters because
/// the flag's whole point is the case a redirected run cannot exercise: on a
/// real terminal `color_enabled` is already true, so `--plain` is the only
/// thing standing between a screen reader and a stream of escape sequences.
fn headless_color(no_color: Option<&std::ffi::OsStr>, stderr_is_tty: bool, plain: bool) -> bool {
    color_enabled(no_color, stderr_is_tty) && !plain
}

/// Everything piped in, or `None` when stdin is a terminal (nobody piped
/// anything and reading would just block waiting for the user to type).
fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    // Lossless-or-nothing: binary on stdin is a mistake worth reporting as
    // "no prompt" rather than smuggling replacement characters to the model.
    std::io::stdin().read_to_string(&mut buffer).ok()?;
    Some(buffer)
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
fn resolve_session(
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

fn new_session_title() -> String {
    format!(
        "New session - {}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    )
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

/// The user's own prompts from a resumed session, most recent first, so the
/// Up key reaches them the way it would in a shell.
///
/// Only messages the *user* typed: a `Role::User` message can also be a
/// carrier for tool results, and recalling `[{"type":"tool_result",...}]` into
/// the prompt box would be nonsense. Text blocks are the only ones a user
/// could have produced by typing.
fn prompt_history(messages: &[Message]) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use smith_tui::ThemeName;
    use std::ffi::OsStr;

    /// The regression: a fresh install used to guess Anthropic and then fail
    /// on a key nobody was asked for.
    #[test]
    fn a_machine_with_nothing_configured_is_a_first_run() {
        assert!(first_run_needed(None, None, None));
    }

    #[test]
    fn any_one_statement_of_intent_ends_the_first_run() {
        assert!(!first_run_needed(Some(ProviderKind::Ollama), None, None));
        assert!(!first_run_needed(None, Some(ProviderKind::Openai), None));
        assert!(!first_run_needed(None, None, Some("openrouter")));
    }

    /// A provider id smith cannot parse is not a configured provider — it is a
    /// typo, and treating it as configured walks into the same dead end one
    /// layer further down, where the message is about a missing key.
    #[test]
    fn a_config_naming_an_unknown_provider_is_still_a_first_run() {
        assert!(first_run_needed(None, None, Some("gpt5-turbo-ultra")));
        assert!(first_run_needed(None, None, Some("")));
    }

    /// The env table has to name the provider each key belongs to, or a box
    /// with only `OPENAI_API_KEY` set gets told it has no *Anthropic* key.
    #[test]
    fn every_provider_key_env_names_its_own_provider() {
        for (var, kind) in PROVIDER_KEY_ENVS {
            let expected = match kind {
                ProviderKind::Anthropic => "ANTHROPIC",
                ProviderKind::Openai => "OPENAI",
                ProviderKind::Openrouter => "OPENROUTER",
                ProviderKind::NineRouter => "NINEROUTER",
                ProviderKind::Ollama => unreachable!("ollama needs no key"),
            };
            assert!(
                var.starts_with(expected),
                "{var} is paired with {kind:?}, which it does not name"
            );
        }
    }

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("smith").chain(args.iter().copied()))
    }

    #[test]
    fn the_flag_surface_is_wired_up_the_way_clap_expects() {
        Cli::command().debug_assert();
    }

    fn config_with_browser(path: Option<&str>) -> Config {
        let mut config = Config::default();
        config.runtime.chromium_path = path.map(str::to_string);
        config
    }

    /// The seam that makes provisioning work at all: `smith_tools::chromium`
    /// only ever looks at these environment variables, so a browser recorded
    /// in config reaches it or it reaches nothing.
    #[test]
    fn a_provisioned_browser_is_handed_to_smith_tools_through_the_env_var() {
        let config = config_with_browser(Some("/home/u/.smith/runtime/chrome-headless-shell"));
        assert_eq!(
            browser_path_to_export(&config, |_| None).as_deref(),
            Some("/home/u/.smith/runtime/chrome-headless-shell")
        );
    }

    /// An explicit override is the user saying which browser to use. Silently
    /// replacing it with smith's own would break the one guarantee those
    /// variables carry.
    #[test]
    fn an_override_the_user_set_is_never_replaced() {
        let config = config_with_browser(Some("/home/u/.smith/runtime/chrome-headless-shell"));
        for var in [
            runtime::BROWSER_PATH_ENV,
            runtime::BROWSER_PATH_ENV_FALLBACK,
        ] {
            let exported =
                browser_path_to_export(&config, |v| (v == var).then(|| "/opt/theirs".to_string()));
            assert_eq!(exported, None, "{var} was overwritten");
        }
    }

    #[test]
    fn nothing_is_exported_when_no_browser_was_provisioned() {
        assert_eq!(
            browser_path_to_export(&config_with_browser(None), |_| None),
            None
        );
        assert_eq!(
            browser_path_to_export(&config_with_browser(Some("   ")), |_| None),
            None
        );
    }

    /// An empty variable is "unset", not "the user chose nothing" — otherwise
    /// a stray `export SMITH_CHROMIUM_PATH=` disables provisioning entirely.
    #[test]
    fn a_blank_override_does_not_suppress_the_provisioned_browser() {
        let config = config_with_browser(Some("/home/u/.smith/runtime/chrome-headless-shell"));
        let exported = browser_path_to_export(&config, |_| Some("  ".to_string()));
        assert!(exported.is_some());
    }

    #[test]
    fn continue_and_resume_are_mutually_exclusive() {
        // Both name a session; accepting both would mean silently picking one.
        assert!(Cli::try_parse_from(["smith", "--continue", "--resume", "abc"]).is_err());
    }

    #[test]
    fn continue_is_spelled_without_the_rust_keyword_escape() {
        let parsed = cli(&["--continue"]);
        assert!(parsed.continue_);
        assert!(parsed.resume.is_none());
    }

    #[test]
    fn sessions_list_defaults_to_a_screenful() {
        let Some(Commands::Sessions {
            action: SessionAction::List { limit },
        }) = cli(&["sessions", "list"]).command
        else {
            panic!("expected sessions list");
        };
        assert_eq!(limit, 20);
    }

    #[test]
    fn sessions_fork_takes_an_optional_cutoff() {
        let Some(Commands::Sessions {
            action: SessionAction::Fork { id, through },
        }) = cli(&["sessions", "fork", "abc", "--through", "4"]).command
        else {
            panic!("expected sessions fork");
        };
        assert_eq!(id, "abc");
        assert_eq!(through, Some(4));

        let Some(Commands::Sessions {
            action: SessionAction::Fork { through, .. },
        }) = cli(&["sessions", "fork", "abc"]).command
        else {
            panic!("expected sessions fork");
        };
        assert_eq!(through, None, "omitting --through copies everything");
    }

    #[test]
    fn sessions_export_defaults_to_markdown() {
        let Some(Commands::Sessions {
            action: SessionAction::Export { format, .. },
        }) = cli(&["sessions", "export", "abc"]).command
        else {
            panic!("expected sessions export");
        };
        assert_eq!(format, ExportFormat::Markdown);
    }

    #[test]
    fn remember_takes_an_unquoted_multi_word_note() {
        let parsed = cli(&["remember", "the", "build", "needs", "nightly"]);
        let Some(Commands::Remember { note, global }) = parsed.command else {
            panic!("expected the remember subcommand, got {:?}", parsed.command);
        };
        assert_eq!(note.join(" "), "the build needs nightly");
        assert!(!global);
    }

    #[test]
    fn remember_rejects_an_empty_note_at_the_parser() {
        // Cheaper than finding out after the file has been opened.
        assert!(Cli::try_parse_from(["smith", "remember"]).is_err());
    }

    #[test]
    fn print_forces_headless_even_on_a_terminal() {
        assert!(cli(&["-p", "hi"]).is_headless(true));
        assert!(cli(&["--print", "hi"]).is_headless(true));
        assert!(cli(&["--plain", "-p", "hi"]).is_headless(true));
        // Asking for a machine-readable format is the same request by another
        // name — a TUI cannot produce one.
        assert!(cli(&["--output-format", "json"]).is_headless(true));
    }

    /// The load-bearing half: a run whose stdout is a pipe or a CI log must
    /// never reach the TUI, whatever the flags say.
    #[test]
    fn a_non_terminal_stdout_forces_headless_on_its_own() {
        assert!(cli(&[]).is_headless(false));
        assert!(!cli(&[]).is_headless(true));
    }

    #[test]
    fn allowed_tools_accepts_commas_and_repetition() {
        let parsed = cli(&["-p", "x", "--allowed-tools", "read_file,run_bash"]);
        assert_eq!(parsed.allowed_tools, ["read_file", "run_bash"]);

        let parsed = cli(&[
            "-p",
            "x",
            "--allowed-tools",
            "read_file",
            "--allowed-tools",
            "run_bash",
        ]);
        assert_eq!(parsed.allowed_tools, ["read_file", "run_bash"]);

        // Nothing listed is the default, and it is what makes the default
        // "deny" rather than "deny only if you said something".
        assert!(cli(&["-p", "x"]).allowed_tools.is_empty());
    }

    #[test]
    fn output_format_uses_the_kebab_case_names_the_docs_promise() {
        assert_eq!(
            cli(&["--output-format", "stream-json"]).output_format,
            Some(OutputFormat::StreamJson)
        );
        assert_eq!(cli(&["-p", "x"]).output_format, None);
    }

    /// `--plain` promises a screen reader "no chrome, no colour escapes".
    ///
    /// The case that matters is the one a redirected run cannot reach: on a
    /// real terminal `color_enabled` is already true, so the flag is the only
    /// thing suppressing the escapes. A test that only ever saw a pipe would
    /// pass for a `--plain` that did nothing at all.
    #[test]
    fn plain_suppresses_colour_on_a_real_terminal() {
        let tty = true;
        assert!(
            headless_color(None, tty, false),
            "a terminal is styled by default"
        );
        assert!(
            !headless_color(None, tty, true),
            "--plain is the whole promise of the flag"
        );
    }

    /// The two ways to ask for the same thing must not disagree.
    #[test]
    fn no_color_and_plain_each_suffice_and_compose() {
        let one = std::ffi::OsString::from("1");
        assert!(!headless_color(Some(&one), true, false), "NO_COLOR alone");
        assert!(!headless_color(None, false, false), "a pipe alone");
        assert!(!headless_color(Some(&one), true, true), "both together");
        // An *empty* NO_COLOR is not a request: the spec is presence-with-value.
        let empty = std::ffi::OsString::new();
        assert!(headless_color(Some(&empty), true, false));
    }

    #[test]
    fn term_dumb_is_treated_as_non_interactive() {
        assert!(term_is_dumb(Some(OsStr::new("dumb"))));
        assert!(!term_is_dumb(Some(OsStr::new("xterm-256color"))));
        assert!(!term_is_dumb(None));
    }

    #[test]
    fn ascii_flag_forces_the_tui_glyph_axis_only() {
        let settings = smith_config::ThemeSettings::default();
        assert!(!tui_theme(true, None, &settings).unwrap().unicode);
        assert_eq!(
            tui_theme(false, None, &settings).unwrap().raised,
            Theme::detect().raised
        );
    }

    #[test]
    fn the_theme_flag_outranks_the_config_and_the_config_outranks_the_default() {
        let mut settings = smith_config::ThemeSettings {
            name: Some("light".into()),
            ..Default::default()
        };
        // Config alone.
        let from_config = tui_theme(false, None, &settings).unwrap();
        assert_eq!(from_config.primary, Theme::named(ThemeName::Light).primary);
        // The flag wins over it.
        let from_flag = tui_theme(false, Some("high_contrast"), &settings).unwrap();
        assert_eq!(
            from_flag.primary,
            Theme::named(ThemeName::HighContrast).primary
        );
        // Neither: the detected default.
        settings.name = None;
        assert_eq!(
            tui_theme(false, None, &settings).unwrap().primary,
            Theme::detect().primary
        );
    }

    #[test]
    fn a_bad_theme_name_or_colour_is_a_usage_error_not_a_fallback() {
        let settings = smith_config::ThemeSettings::default();
        let err = tui_theme(false, Some("solarized"), &settings).unwrap_err();
        assert!(
            err.contains("solarized") && err.contains("high_contrast"),
            "{err}"
        );

        let mut settings = smith_config::ThemeSettings::default();
        settings
            .colors
            .insert("ember".into(), "not-a-colour".into());
        let err = tui_theme(false, None, &settings).unwrap_err();
        assert!(err.contains("ember"), "{err}");
    }

    #[test]
    fn a_per_token_override_survives_into_the_resolved_theme() {
        let mut settings = smith_config::ThemeSettings::default();
        settings.colors.insert("base".into(), "#123456".into());
        let theme = tui_theme(false, Some("dark"), &settings).unwrap();
        assert_ne!(theme.base, Theme::detect().base);
    }

    #[test]
    fn no_color_is_respected_only_when_it_is_actually_set_to_something() {
        assert!(color_enabled(None, true));
        assert!(!color_enabled(Some(OsStr::new("1")), true));
        // Per no-color.org an empty value does not count.
        assert!(color_enabled(Some(OsStr::new("")), true));
        // Nothing is a terminal in a pipeline, whatever NO_COLOR says.
        assert!(!color_enabled(None, false));
    }
}
