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
