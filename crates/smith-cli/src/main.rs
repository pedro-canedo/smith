use frontend::{is_dumb_terminal, run_headless, run_tui};
use startup::{needs_first_run_setup, restore_default_sigpipe};
use std::io::IsTerminal;
use std::process::ExitCode;
use subcommands::{run_remember, run_sessions};

use clap::{Parser, Subcommand};

use headless::{OutputFormat, EXIT_OK, EXIT_TURN_FAILED, EXIT_USAGE};
use orchestrator::ProviderKind;

#[path = "main/frontend.rs"]
mod frontend;
#[path = "main/startup.rs"]
mod startup;
#[path = "main/subcommands.rs"]
mod subcommands;

mod doctor;
mod headless;
mod logging;
mod node_runtime;
mod orchestrator;
mod preflight;
mod prompts;
mod resources;
mod runtime;
mod setup;
mod subagents;
mod update;
mod webconfig;

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
    Doctor {
        /// Install what is missing instead of only reporting it, asking
        /// before each download. Refuses in a non-interactive shell rather
        /// than fetching ~200 MB into somebody's CI job unasked.
        #[arg(long)]
        fix: bool,
    },
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
    /// Configure smith in a browser instead of the terminal.
    ///
    /// Serves a page on 127.0.0.1 with an ephemeral port and a one-time key
    /// in the URL, and stops when you are done with it. The terminal wizard
    /// stays the primary path; this is for people who would rather not use
    /// one, and it works over `ssh -L` by pasting the printed link.
    Web {
        /// Print the URL but do not try to open anything.
        #[arg(long)]
        no_browser: bool,

        /// Listen on a fixed port instead of asking the OS for a free one.
        /// Only useful for `ssh -L`; a predictable port is otherwise the one
        /// property a local credential endpoint should not have.
        #[arg(long)]
        port: Option<u16>,
    },
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
        // Before `--cwd`, with the rest of `setup`: it writes the *global*
        // config, so which project directory this run is about is not its
        // business.
        if let Some(SetupResource::Web { no_browser, port }) = resource {
            // A terminal is required even though the UI is not: the URL
            // carries the key, and printing it into a pipe means either
            // nobody reads it or something logs it.
            if !std::io::stdout().is_terminal() {
                eprintln!(
                    "smith: `setup web` needs a terminal to show you the link — use `smith setup`."
                );
                return ExitCode::from(EXIT_USAGE);
            }
            return match webconfig::run(*no_browser, *port).await {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(e) => {
                    eprintln!("smith: {e}");
                    ExitCode::from(EXIT_USAGE)
                }
            };
        }
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
    if let Some(Commands::Doctor { fix }) = &command {
        return ExitCode::from(doctor::run(*fix).await);
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

// `#[path]` because `main.rs` is a crate root: a bare `mod tests;` would
// resolve to `src/tests.rs`, sitting among the real modules as if it were
// one. The tests belong beside the file they cover, so the path says so.
#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;
