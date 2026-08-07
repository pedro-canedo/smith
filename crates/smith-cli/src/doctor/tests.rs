use super::*;

fn config_from(toml_text: &str) -> Config {
    toml::from_str(toml_text).unwrap()
}

// -- status and exit code ----------------------------------------------

#[test]
fn an_all_ok_report_exits_zero() {
    let mut report = Report::default();
    report.push(Check::ok("a", "fine"));
    report.push(Check::ok("b", "fine"));
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.worst(), Status::Ok);
}

/// A warning is a degraded setup, not a broken one. Exiting non-zero for
/// "no browser for web_search" would make `smith doctor` unusable as a CI
/// gate, which is the entire reason it has an exit code.
#[test]
fn warnings_alone_still_exit_zero() {
    let mut report = Report::default();
    report.push(Check::ok("a", "fine"));
    report.push(Check::warn("b", "degraded", "do the thing"));
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.worst(), Status::Warn);
}

#[test]
fn any_failure_exits_non_zero() {
    let mut report = Report::default();
    report.push(Check::ok("a", "fine"));
    report.push(Check::warn("b", "degraded", "do the thing"));
    report.push(Check::fail("c", "broken", "fix the thing"));
    assert_eq!(report.exit_code(), EXIT_FAILED);
    assert_ne!(report.exit_code(), 0);
    assert_eq!(report.worst(), Status::Fail);
}

#[test]
fn an_empty_report_is_not_a_failure() {
    assert_eq!(Report::default().exit_code(), 0);
}

// -- rendering ----------------------------------------------------------

#[test]
fn renders_a_status_a_name_and_a_detail_for_every_check() {
    let mut report = Report::default();
    report.push(Check::ok("config", "layers in effect: global"));
    report.push(Check::fail(
        "api key",
        "no key for anthropic",
        "Run `smith setup`.",
    ));
    let text = report.render(&Redactor::default());

    assert!(text.contains("OK   config"), "{text}");
    assert!(text.contains("layers in effect: global"), "{text}");
    assert!(text.contains("FAIL api key"), "{text}");
    assert!(text.contains("-> Run `smith setup`."), "{text}");
}

#[test]
fn the_summary_counts_failures_and_warnings() {
    let mut report = Report::default();
    report.push(Check::ok("a", "fine"));
    report.push(Check::warn("b", "meh", "r"));
    report.push(Check::fail("c", "bad", "r"));
    let text = report.render(&Redactor::default());
    assert!(
        text.contains("3 checks, 1 failure(s), 1 warning(s)."),
        "{text}"
    );
}

#[test]
fn an_all_ok_report_says_so() {
    let mut report = Report::default();
    report.push(Check::ok("a", "fine"));
    assert!(report
        .render(&Redactor::default())
        .contains("1 checks, all OK."));
}

#[test]
fn warnings_without_failures_are_summarised_as_such() {
    let mut report = Report::default();
    report.push(Check::warn("a", "meh", "r"));
    assert!(report
        .render(&Redactor::default())
        .contains("1 checks, 1 warning(s), no failures."));
}

/// A multi-line remedy has to stay attached to its check rather than
/// running back to the left margin — the Ollama install remedy is two
/// lines, and the second is the important one.
#[test]
fn a_multi_line_remedy_stays_indented_under_its_check() {
    let mut report = Report::default();
    report.push(Check::fail("ollama", "not installed", "line one\nline two"));
    let text = report.render(&Redactor::default());
    assert!(text.contains("       -> line one\n"), "{text}");
    assert!(text.contains("       -> line two\n"), "{text}");
}

// -- the invariant this module exists for -------------------------------

/// Runs the whole diagnosis against a deliberately broken configuration
/// and holds every non-OK result to the rule. This is what catches a new
/// check added without a remedy — the failure mode the module docs call a
/// bug.
#[tokio::test]
async fn no_check_in_a_real_run_fails_without_telling_you_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_from(
        r#"
            [general]
            provider = "ollama"
            [ollama]
            base_url = "http://127.0.0.1:1/v1"
            [[mcp_servers]]
            name = "definitely-not-installed"
            command = "smith-doctor-no-such-command"
            "#,
    );

    let report = diagnose(dir.path(), &config).await;

    assert!(!report.checks.is_empty());
    for check in &report.checks {
        if check.status == Status::Ok {
            continue;
        }
        let remedy = check.remedy.as_deref().unwrap_or("");
        assert!(
            remedy.trim().len() > 20,
            "check `{}` ({:?}) has no usable remedy: {remedy:?}",
            check.name,
            check.status
        );
    }
    // An unreachable provider and a missing MCP server are both failures,
    // so this run has to be usable as a CI red.
    assert_eq!(report.exit_code(), EXIT_FAILED);
}

/// Every check the run produced is named, so the report can't quietly
/// stop covering one of the things the spec lists.
#[tokio::test]
async fn the_report_covers_every_area_the_spec_names() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_from(
        r#"
            [[mcp_servers]]
            name = "x"
            command = "smith-doctor-no-such-command"
            "#,
    );

    let report = diagnose(dir.path(), &config).await;
    let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();

    for expected in [
        "config",
        "provider",
        "api key",
        "provider reachable",
        "ollama",
        "web_search browser",
        ".smith dir",
        "session db",
    ] {
        assert!(
            names.contains(&expected),
            "missing `{expected}` in {names:?}"
        );
    }
    assert!(names.iter().any(|n| n.starts_with("mcp:")), "{names:?}");
}

// -- secrets ------------------------------------------------------------

/// Where the key came from, never the key itself.
#[test]
fn the_api_key_check_names_its_source_and_never_the_key() {
    const KEY: &str = "sk-ant-api03-super-secret-value";
    let config = config_from(&format!("[anthropic]\napi_key = \"{KEY}\""));
    let resolved = resolve_api_key(&config, ProviderKind::Anthropic);
    // Only meaningful when the environment isn't supplying one instead.
    if resolved.source() != &KeySource::ConfigFile {
        return;
    }

    let check = check_api_key(ProviderKind::Anthropic, &resolved);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("config"), "{}", check.detail);
    assert!(!check.detail.contains(KEY), "leaked: {}", check.detail);
}

/// Belt and braces: even if a future check interpolated a key into a
/// detail, `render` must not put it on stdout.
#[test]
fn the_redactor_scrubs_a_key_that_leaked_into_a_detail() {
    const KEY: &str = "sk-ant-api03-super-secret-value";
    let mut report = Report::default();
    report.push(Check::fail("careless", format!("key was {KEY}"), "fix it"));

    let text = report.render(&Redactor::new([KEY.to_string()]));

    assert!(!text.contains(KEY), "leaked: {text}");
    assert!(text.contains("[redacted]"), "{text}");
}

/// With no key at all, the missing key is the one diagnosis worth making.
/// A second FAIL saying the key is "wrong, expired, or lacks access" would
/// send someone to regenerate a credential they never had.
#[tokio::test]
async fn an_unauthenticated_probe_defers_to_the_api_key_check() {
    let config = config_from("[general]\nprovider = \"anthropic\"");
    let check = check_provider_reachable(&config, ProviderKind::Anthropic, None).await;
    if check.detail.contains("cannot reach") {
        // Offline; nothing to assert about the endpoint's answer.
        return;
    }
    assert_ne!(check.status, Status::Fail, "{}", check.detail);
    assert!(
        check.remedy.as_deref().unwrap_or("").contains("api key"),
        "{check:?}"
    );
}

#[test]
fn a_missing_key_is_a_failure_that_names_the_variable_to_set() {
    let key = ResolvedKey {
        source: KeySource::Missing,
        value: None,
    };
    let check = check_api_key(ProviderKind::Anthropic, &key);
    assert_eq!(check.status, Status::Fail);
    assert!(check.remedy.as_ref().unwrap().contains("ANTHROPIC_API_KEY"));

    let check = check_api_key(ProviderKind::Openai, &key);
    assert!(check.remedy.as_ref().unwrap().contains("OPENAI_API_KEY"));
}

/// Ollama has no credential; reporting a missing key would send someone
/// hunting for one that does not exist.
#[test]
fn ollama_needs_no_key_and_is_not_reported_as_missing_one() {
    let config = config_from("[general]\nprovider = \"ollama\"");
    let resolved = resolve_api_key(&config, ProviderKind::Ollama);
    assert_eq!(resolved.source(), &KeySource::NotNeeded);
    assert_eq!(
        check_api_key(ProviderKind::Ollama, &resolved).status,
        Status::Ok
    );
}

/// `build_provider` prefers the environment, so doctor has to report the
/// key that will actually be used rather than the other one.
#[test]
fn the_environment_wins_over_the_config_file_just_as_it_does_at_runtime() {
    let config = config_from("[anthropic]\napi_key = \"from-config-file-value\"");
    let resolved = resolve_api_key(&config, ProviderKind::Anthropic);
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(v) if !v.trim().is_empty() => {
            assert_eq!(resolved.source(), &KeySource::Env("ANTHROPIC_API_KEY"))
        }
        _ => assert_eq!(resolved.source(), &KeySource::ConfigFile),
    }
}

// -- provider selection -------------------------------------------------

#[test]
fn an_unknown_provider_name_is_a_failure_not_a_silent_fallback() {
    let config = config_from("[general]\nprovider = \"claude-but-typoed\"");
    let check = check_provider_selected(&config, resolve_provider(&config));
    assert_eq!(check.status, Status::Fail);
    assert!(check
        .remedy
        .as_ref()
        .unwrap()
        .contains("anthropic, openai, openrouter, 9router, ollama"));
}

#[test]
fn a_configured_provider_and_model_are_reported_together() {
    let config = config_from("[general]\nprovider = \"openai\"\nmodel = \"gpt-4.1\"");
    let check = check_provider_selected(&config, resolve_provider(&config));
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("openai"), "{}", check.detail);
    assert!(check.detail.contains("gpt-4.1"), "{}", check.detail);
}

#[test]
fn no_configured_provider_warns_and_names_the_default() {
    let config = Config::default();
    let check = check_provider_selected(&config, resolve_provider(&config));
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("anthropic"), "{}", check.detail);
}

// -- config layers ------------------------------------------------------

#[test]
fn an_unparseable_project_config_is_a_failure_that_says_which_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = smith_config::project_config_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "this is [not valid toml").unwrap();

    let check = check_config_layers(dir.path());

    assert_eq!(check.status, Status::Fail);
    assert!(
        check.detail.contains(&path.display().to_string()),
        "{}",
        check.detail
    );
    // The specific danger: it is ignored rather than fatal, so smith runs
    // with settings the user did not intend.
    assert!(check.remedy.as_ref().unwrap().contains("ignores"));
}

#[test]
fn a_valid_project_config_is_listed_as_a_layer_in_effect() {
    let dir = tempfile::tempdir().unwrap();
    let path = smith_config::project_config_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "[general]\nmodel = \"x\"\n").unwrap();

    let check = check_config_layers(dir.path());

    assert_ne!(check.status, Status::Fail);
    assert!(check.detail.contains("project"), "{}", check.detail);
}

// -- ollama guidance ----------------------------------------------------

/// The remedy has to be the command for the machine in front of the user,
/// not a link to a page listing five of them.
#[test]
fn the_ollama_remedy_is_a_command_for_the_detected_platform() {
    assert!(ollama_install_command("macos").contains("brew install ollama"));
    assert!(ollama_install_command("linux").contains("ollama.com/install.sh"));
    assert!(ollama_install_command("windows").contains("winget"));
    // An unknown platform still gets somewhere to go.
    assert!(ollama_install_command("plan9").contains("ollama.com/download"));
}

/// Installing Ollama is guidance, never an action: it registers a daemon,
/// edits PATH and may need sudo. The remedy has to carry the command, and
/// smith has to not run it.
#[tokio::test]
async fn a_missing_ollama_is_explained_rather_than_installed() {
    let config = config_from("[general]\nprovider = \"ollama\"");
    let check = check_ollama(&config).await;
    if check.status == Status::Ok {
        // This machine has Ollama; there is no advice to assert about.
        return;
    }
    let remedy = check.remedy.as_ref().unwrap();
    assert!(
        remedy.contains("ollama serve") || remedy.contains("Install it"),
        "{remedy}"
    );
    assert!(
        remedy.contains("will not install it") || remedy.contains("ollama serve"),
        "{remedy}"
    );
}

// -- project directory --------------------------------------------------

#[test]
fn a_writable_project_dir_and_an_existing_db_both_pass() {
    let dir = tempfile::tempdir().unwrap();
    // History lives centrally now, so "a project that has been used" means a
    // database under `~/.smith/projects/<id>/`. Pointed at a temporary
    // directory here so the test never writes into the real one.
    let store = dir.path().join("central");
    drop(smith_store::SessionStore::open(&store).unwrap());

    let (writable, schema) = check_project_dir_at(dir.path(), &store);

    assert_eq!(writable.status, Status::Ok, "{}", writable.detail);
    assert_eq!(schema.status, Status::Ok, "{}", schema.detail);
    // The version is what a future `SchemaTooNew` gets compared against.
    assert!(
        schema.detail.contains("schema version"),
        "{}",
        schema.detail
    );
}

/// Regression, and the reason `probe_writable` no longer calls
/// `create_dir_all`: an earlier version created `.smith/` and a
/// `sessions.db` in whatever directory doctor ran in. That is not merely
/// untidy — a stray `.smith/` is a marker `MemoryScope::discover` treats
/// as a project root, so *diagnosing* a directory changed how smith
/// behaved in it afterwards.
#[test]
fn diagnosing_a_project_creates_nothing_in_it() {
    let dir = tempfile::tempdir().unwrap();

    let (writable, schema) = check_project_dir(dir.path());

    assert_eq!(writable.status, Status::Ok, "{}", writable.detail);
    assert_eq!(schema.status, Status::Ok, "{}", schema.detail);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "doctor created: {leftovers:?}");
}

/// A project with no history is healthy, not broken — and saying so is
/// what lets the check avoid creating a database to find out.
#[test]
fn a_project_with_no_history_yet_is_reported_as_fine() {
    let dir = tempfile::tempdir().unwrap();
    let check = check_session_db(dir.path());
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("none yet"), "{}", check.detail);
    assert!(!dir.path().join(".smith").exists());
}

/// The probe must not leave its own droppings in an existing `.smith`.
#[test]
fn the_writability_probe_cleans_up_after_itself() {
    let dir = tempfile::tempdir().unwrap();
    let smith_dir = dir.path().join(".smith");
    std::fs::create_dir_all(&smith_dir).unwrap();

    probe_writable(&smith_dir).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(&smith_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[test]
#[cfg(unix)]
fn an_unwritable_project_dir_fails_and_the_db_check_does_not_pile_on() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let smith_dir = dir.path().join(".smith");
    std::fs::create_dir_all(&smith_dir).unwrap();
    std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let (writable, schema) = check_project_dir(dir.path());

    // Running as root defeats the mode bits entirely; skip rather than
    // assert something untrue about the check.
    if writable.status == Status::Ok {
        let _ = std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o700));
        return;
    }
    assert_eq!(writable.status, Status::Fail);
    assert!(writable.remedy.as_ref().unwrap().contains("chmod"));
    // A second failure would be noise: it has the same single cause.
    assert_eq!(schema.status, Status::Warn);

    let _ = std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o700));
}

// -- mcp ----------------------------------------------------------------

#[tokio::test]
async fn an_mcp_server_that_is_not_installed_says_which_command_is_missing() {
    let check = check_mcp_server(&McpServerConfig {
        name: "ghost".to_string(),
        command: "smith-doctor-definitely-not-a-real-command".to_string(),
        ..Default::default()
    })
    .await;

    assert_eq!(check.status, Status::Fail);
    assert_eq!(check.name, "mcp:ghost");
    assert!(check
        .detail
        .contains("smith-doctor-definitely-not-a-real-command"));
    let remedy = check.remedy.as_ref().unwrap();
    assert!(remedy.contains("PATH"), "{remedy}");
    assert!(remedy.contains("ghost"), "{remedy}");
}

#[tokio::test]
async fn no_configured_mcp_servers_is_a_clean_ok_rather_than_a_missing_check() {
    let mut report = Report::default();
    report.extend_mcp(&Config::default()).await;
    assert_eq!(report.checks.len(), 1);
    assert_eq!(report.checks[0].status, Status::Ok);
}

/// A minimal MCP server, so the happy path is exercised rather than only
/// the failures — otherwise "does it start and list tools" is untested.
const FAKE_SERVER: &str = r#"
import sys, json
def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    m = req.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": TOOLS}})
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

fn fake_server(tools_json: &str) -> McpServerConfig {
    McpServerConfig {
        name: "fake".to_string(),
        command: "python3".to_string(),
        args: vec!["-c".to_string(), FAKE_SERVER.replace("TOOLS", tools_json)],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_working_mcp_server_is_reported_with_the_tools_it_publishes() {
    if !python3_available() {
        return;
    }
    let check = check_mcp_server(&fake_server(
        r#"[{"name": "alpha", "description": "a", "inputSchema": {"type": "object"}},
                {"name": "beta", "description": "b", "inputSchema": {"type": "object"}}]"#,
    ))
    .await;

    assert_eq!(check.status, Status::Ok, "{}", check.detail);
    assert!(check.detail.contains("2 tool(s)"), "{}", check.detail);
    assert!(check.detail.contains("alpha"), "{}", check.detail);
    assert!(check.remedy.is_none());
}

/// A server that starts but publishes nothing is a real and confusing
/// state — usually missing credentials — and earns its own advice rather
/// than a green tick.
#[tokio::test]
async fn an_mcp_server_publishing_no_tools_warns_rather_than_passing() {
    if !python3_available() {
        return;
    }
    let check = check_mcp_server(&fake_server("[]")).await;

    assert_eq!(check.status, Status::Warn, "{}", check.detail);
    assert!(check.remedy.as_ref().unwrap().contains("credentials"));
}

// -- the browser check --------------------------------------------------

#[tokio::test]
async fn a_configured_browser_that_does_not_exist_warns_with_a_fix() {
    let mut config = Config::default();
    config.runtime.chromium_path = Some("/nonexistent/chrome-headless-shell".to_string());

    let check = check_browser(&config).await;

    // Only meaningful when no env override is shadowing the config value.
    if check.detail.contains("nonexistent") {
        assert_eq!(check.status, Status::Warn);
        assert!(check.remedy.as_ref().unwrap().contains("smith setup"));
    }
}

/// No browser at all is a warning, never a failure: `web_search` still
/// works over plain HTTP, so a machine without one must not go red.
#[tokio::test]
async fn a_missing_browser_is_a_warning_not_a_failure() {
    assert_ne!(check_browser(&Config::default()).await.status, Status::Fail);
}

// ---- the ollama cloud check ------------------------------------------------

fn linked(names: &[(&str, bool)]) -> Vec<smith_provider::OllamaModel> {
    names
        .iter()
        .map(|(name, is_cloud)| smith_provider::OllamaModel {
            name: (*name).to_string(),
            is_cloud: *is_cloud,
            context_window: None,
            size_bytes: None,
            supports_tools: true,
        })
        .collect()
}

/// The configured model is what a turn will use, so its entitlement is the
/// fact worth reporting.
#[test]
fn the_configured_cloud_model_is_the_one_probed() {
    let models = linked(&[("nemotron-3-super:cloud", true), ("qwen3.5:9b", false)]);
    assert_eq!(
        pick_cloud_probe("gpt-oss:20b-cloud", &models).as_deref(),
        Some("gpt-oss:20b-cloud"),
        "even when it is not linked yet — that is still what a turn asks for"
    );
}

/// With a local model configured, the check is only asking "is this daemon
/// signed in". Probing a paid link would answer a second question nobody
/// asked, and its refusal reads as a problem with smith rather than as a fact
/// about an unused link.
#[test]
fn a_free_cloud_model_is_preferred_for_the_signin_probe() {
    let models = linked(&[
        ("deepseek-v4-flash:cloud", true),
        ("nemotron-3-super:cloud", true),
    ]);
    assert_eq!(
        pick_cloud_probe("qwen3.5:9b", &models).as_deref(),
        Some("nemotron-3-super:cloud"),
        "the free one isolates the question"
    );
}

/// With nothing free linked, any cloud model still answers the signin
/// question — a partial answer beats none.
#[test]
fn any_cloud_model_is_used_when_none_of_the_free_ones_are_linked() {
    let models = linked(&[("deepseek-v4-flash:cloud", true)]);
    assert_eq!(
        pick_cloud_probe("qwen3.5:9b", &models).as_deref(),
        Some("deepseek-v4-flash:cloud")
    );
}

/// A machine running local weights must not be told about an account it does
/// not need — the check does not run at all.
#[test]
fn a_machine_with_no_cloud_model_is_not_asked_about_one() {
    let models = linked(&[("qwen3.5:9b", false), ("llama3.3", false)]);
    assert_eq!(pick_cloud_probe("qwen3.5:9b", &models), None);
    assert_eq!(pick_cloud_probe("", &[]), None);
}
