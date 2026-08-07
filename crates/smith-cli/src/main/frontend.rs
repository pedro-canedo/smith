//! The two frontends, and the terminal policy that picks between them.

use crate::startup::{
    channels, detect_git_branch, display_path, last_write_tasks_call, messages_to_chat_lines,
    prompt_history, Startup,
};
use std::collections::BTreeSet;
use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use smith_tui::{ChatLine, ChatRole, IdleHint, Theme, TuiConfig};

use headless::{HeadlessOptions, EXIT_OK, EXIT_TURN_FAILED, EXIT_USAGE};
use orchestrator::{run_orchestrator, OrchestratorOptions, ProviderKind};

use super::*;

pub(crate) async fn run_tui(cli: Cli, logs: smith_tui::LogBuffer) -> ExitCode {
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

    let web_enabled = cli.web || startup.config.web.enabled.unwrap_or(false);
    // The console URL names the session, so a fresh session needs its id
    // *now* rather than lazily on the first message. Persistence keeps its
    // lazy row creation — `ensure_session_id` reuses this id and only
    // touches the database when something is actually persisted.
    if web_enabled && startup.session_id.is_none() {
        startup.session_id = Some(uuid::Uuid::new_v4().to_string());
    }

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

    let mut tui_config = TuiConfig {
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
        // Filled in below once the console server has a port.
        console_url: None,
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

    // The broker owns the ask oneshots for every interactive frontend — see
    // `askbroker`. Spawned before the orchestrator so no ask can arrive with
    // nobody listening.
    let (ask_tx, ask_rx) = tokio::sync::mpsc::unbounded_channel();
    let broker = tokio::spawn(crate::askbroker::run(
        permission_rx,
        question_rx,
        ask_rx,
        chans.event_tx.clone(),
    ));

    // With the console off, `event_rx` goes to the TUI untouched — this
    // path is byte-identical to a smith without a web console. With it on,
    // the pump tees events into the projection and the SSE broadcast on
    // their way to the TUI.
    let mut pump_task = None;
    let mut server_task = None;
    let event_rx = if web_enabled {
        let session_id = startup
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tee = webconsole::state::Tee::new(
            session_id.clone(),
            startup.provider_kind.label().to_string(),
            startup.model.clone(),
        );
        let (to_tui, tui_rx) = tokio::sync::mpsc::unbounded_channel();
        pump_task = Some(tokio::spawn(webconsole::state::pump(
            event_rx,
            to_tui,
            tee.clone(),
        )));

        let port = cli.web_port.or(startup.config.web.port).unwrap_or(0);
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => match listener.local_addr() {
                Ok(addr) => {
                    let guard = std::sync::Arc::new(crate::webguard::Guard {
                        host: format!("127.0.0.1:{}", addr.port()),
                        token: crate::webguard::mint_token(),
                    });
                    let url = format!("http://{}/s/{}?t={}", guard.host, session_id, guard.token);
                    let handles = webconsole::Handles {
                        guard,
                        tee,
                        action_tx: action_tx.clone(),
                        ask_tx: ask_tx.clone(),
                        session_id,
                        // Empty when the project store cannot be located;
                        // history endpoints then answer 500 per request
                        // rather than the console refusing to start.
                        store_dir: smith_config::project_store_dir(&startup.cwd)
                            .unwrap_or_default(),
                    };
                    server_task = Some(tokio::spawn(webconsole::server::serve(
                        listener,
                        handles,
                        webconsole::PAGE,
                    )));
                    if startup.config.web.open_browser.unwrap_or(false) {
                        crate::webconfig::open_browser(&url);
                    }
                    tui_config.console_url = Some(url);
                }
                Err(e) => eprintln!("smith: web console could not read its address: {e}"),
            },
            Err(e) => eprintln!("smith: web console could not listen on 127.0.0.1: {e}"),
        }
        tui_rx
    } else {
        event_rx
    };

    let orchestrator = tokio::spawn(run_orchestrator(opts, chans));

    let result = smith_tui::run(tui_config, action_tx, event_rx, ask_tx).await;
    orchestrator.abort();
    broker.abort();
    // The console dies with the session — that is its whole lifetime story
    // (docs/web-console.md §4.2). EventSource reconnects then fail and the
    // page shows "session ended".
    if let Some(server) = server_task {
        server.abort();
    }
    if let Some(pump) = pump_task {
        pump.abort();
    }
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

pub(crate) async fn run_headless(cli: Cli) -> u8 {
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
    let provider = match orchestrator::build_provider_stack(
        startup.provider_kind,
        &startup.config,
        &startup.model,
    ) {
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
pub(crate) fn tui_theme(
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

pub(crate) fn is_dumb_terminal() -> bool {
    term_is_dumb(std::env::var_os("TERM").as_deref())
}

pub(crate) fn term_is_dumb(term: Option<&std::ffi::OsStr>) -> bool {
    term.and_then(std::ffi::OsStr::to_str) == Some("dumb")
}

/// Whether the text format may use ANSI colour.
///
/// `NO_COLOR` (https://no-color.org) wins when set to anything non-empty — the
/// spec is explicit that an empty value does *not* count, so `NO_COLOR=` set
/// by an over-eager wrapper script doesn't silently strip colour. A
/// non-terminal stderr disables it too: the chrome lives on stderr, so that is
/// the stream whose capabilities decide, not stdout's.
pub(crate) fn color_enabled(no_color: Option<&std::ffi::OsStr>, stderr_is_tty: bool) -> bool {
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
pub(crate) fn headless_color(
    no_color: Option<&std::ffi::OsStr>,
    stderr_is_tty: bool,
    plain: bool,
) -> bool {
    color_enabled(no_color, stderr_is_tty) && !plain
}

/// Everything piped in, or `None` when stdin is a terminal (nobody piped
/// anything and reading would just block waiting for the user to type).
pub(crate) fn read_piped_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    // Lossless-or-nothing: binary on stdin is a mistake worth reporting as
    // "no prompt" rather than smuggling replacement characters to the model.
    std::io::stdin().read_to_string(&mut buffer).ok()?;
    Some(buffer)
}
