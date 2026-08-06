use super::*;

#[test]
fn create_append_and_load_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();

    let id = store
        .create_session("anthropic", "claude-sonnet-5", "/tmp/proj")
        .unwrap();
    store
        .append_message(&id, &Message::user_text("hello there smith"))
        .unwrap();
    store
        .append_message(
            &id,
            &Message::assistant(vec![ContentBlock::Text { text: "hi!".into() }]),
        )
        .unwrap();

    let loaded = store.load_messages(&id).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, Role::User);
    assert_eq!(loaded[1].text(), "hi!");

    let latest = store.latest_session().unwrap().unwrap();
    assert_eq!(latest.id, id);
    assert_eq!(latest.title, "hello there smith");
}

#[test]
fn ensure_session_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = "fixed-session-id";
    store
        .ensure_session(id, "ollama", "qwen", "/tmp/proj")
        .unwrap();
    store
        .ensure_session(id, "ollama", "qwen", "/tmp/proj")
        .unwrap();
    store.append_message(id, &Message::user_text("hi")).unwrap();
    let loaded = store.load_messages(id).unwrap();
    assert_eq!(loaded.len(), 1);
}

#[test]
fn goal_round_trips_per_session_and_does_not_leak_to_others() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let a = store
        .create_session("anthropic", "claude-sonnet-5", "/tmp/proj")
        .unwrap();
    let b = store
        .create_session("anthropic", "claude-sonnet-5", "/tmp/proj")
        .unwrap();

    assert_eq!(store.load_goal(&a).unwrap(), None);

    store.set_goal(&a, Some("ship the login page")).unwrap();
    assert_eq!(
        store.load_goal(&a).unwrap().as_deref(),
        Some("ship the login page")
    );
    // A different session in the same project must not see it.
    assert_eq!(store.load_goal(&b).unwrap(), None);

    store.set_goal(&a, None).unwrap();
    assert_eq!(store.load_goal(&a).unwrap(), None);
}

#[test]
fn latest_session_is_none_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    assert!(store.latest_session().unwrap().is_none());
}

#[test]
fn latest_session_picks_most_recently_updated() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();

    let first = store
        .create_session("anthropic", "claude-sonnet-5", "/tmp/proj")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = store
        .create_session("anthropic", "claude-sonnet-5", "/tmp/proj")
        .unwrap();
    store
        .append_message(&second, &Message::user_text("second session"))
        .unwrap();

    let latest = store.latest_session().unwrap().unwrap();
    assert_eq!(latest.id, second);
    assert_ne!(latest.id, first);
}

/// An unknown id and a session that simply has no messages are
/// indistinguishable through `load_messages` — which is exactly why
/// `--resume <typo>` used to look like a successful resume.
#[test]
fn session_exists_distinguishes_unknown_ids_from_empty_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("anthropic", "m", "/tmp").unwrap();

    assert!(store.session_exists(&id).unwrap());
    assert!(!store.session_exists("no-such-session").unwrap());

    // Both come back empty, which is the trap.
    assert!(store.load_messages(&id).unwrap().is_empty());
    assert!(store.load_messages("no-such-session").unwrap().is_empty());
}

// ---- migrations --------------------------------------------------------

/// The schema exactly as a pre-versioning smith created it: `sessions`
/// without a `goal` column, no `schema_version` table, no `turns` table.
const LEGACY_V0_SCHEMA: &str = "
        CREATE TABLE IF NOT EXISTS sessions (
            id         TEXT PRIMARY KEY,
            title      TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            provider   TEXT NOT NULL,
            model      TEXT NOT NULL,
            cwd        TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq        INTEGER NOT NULL,
            role       TEXT NOT NULL,
            content    TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, seq);
        CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
    ";

/// Builds a database the old way — bypassing `SessionStore::open` entirely
/// — with one session and one message already in it. `extra` is applied
/// afterwards, to model the second legacy shape (the ad-hoc `goal`
/// `ALTER` having already run).
fn legacy_v0_database(dir: &Path, extra: Option<&str>) {
    let smith_dir = dir.join(".smith");
    std::fs::create_dir_all(&smith_dir).unwrap();
    let conn = Connection::open(smith_dir.join("sessions.db")).unwrap();
    conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    if let Some(extra) = extra {
        conn.execute_batch(extra).unwrap();
    }
    conn.execute(
            "INSERT INTO sessions (id, title, created_at, updated_at, provider, model, cwd)
             VALUES ('legacy', 'an old conversation', 1, 2, 'anthropic', 'claude-sonnet-5', '/proj')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO messages (session_id, seq, role, content, created_at)
             VALUES ('legacy', 0, 'user', ?1, 3)",
        params![serde_json::to_string(&Message::user_text("do not lose me").content).unwrap()],
    )
    .unwrap();
}

/// The migration path's whole reason for existing: a database out in the
/// wild, created before any of this, has to keep working — and keep its
/// rows.
#[test]
fn a_version_zero_database_migrates_without_losing_anything() {
    let dir = tempfile::tempdir().unwrap();
    legacy_v0_database(dir.path(), None);

    let store = SessionStore::open(dir.path()).unwrap();

    // Detected as version 0 (no `schema_version` table) and brought all
    // the way up.
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    // The data is untouched.
    let messages = store.load_messages("legacy").unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text(), "do not lose me");
    let latest = store.latest_session().unwrap().unwrap();
    assert_eq!(latest.id, "legacy");
    assert_eq!(latest.title, "an old conversation");

    // Migration 2 gave it the column it never had...
    store
        .set_goal("legacy", Some("finish the old task"))
        .unwrap();
    assert_eq!(
        store.load_goal("legacy").unwrap().as_deref(),
        Some("finish the old task")
    );
    // ...and migration 3 gave it the table it never had.
    store
        .record_turn(
            "legacy",
            &TurnRecord {
                provider: "anthropic".into(),
                model: "claude-sonnet-5".into(),
                usage: Usage {
                    input_tokens: 10,
                    ..Usage::default()
                },
                cost_usd: Some(0.5),
            },
        )
        .unwrap();
    assert_eq!(store.turn_totals("legacy").unwrap().turns, 1);
}

/// The other legacy shape: a database that already ran the old ad-hoc
/// `ALTER TABLE ... ADD COLUMN goal`. Both are version 0 and
/// indistinguishable from the outside, which is why migration 2 checks
/// rather than swallowing an error.
#[test]
fn a_version_zero_database_that_already_has_the_goal_column_still_migrates() {
    let dir = tempfile::tempdir().unwrap();
    legacy_v0_database(
        dir.path(),
        Some("ALTER TABLE sessions ADD COLUMN goal TEXT;"),
    );

    let store = SessionStore::open(dir.path()).unwrap();

    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.load_messages("legacy").unwrap().len(), 1);
    store.set_goal("legacy", Some("still works")).unwrap();
    assert_eq!(
        store.load_goal("legacy").unwrap().as_deref(),
        Some("still works")
    );
}

#[test]
fn a_fresh_database_lands_on_the_current_version_and_reopening_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let id = store.create_session("anthropic", "m", "/proj").unwrap();
    drop(store);

    // Reopening must not re-run anything or duplicate a version row.
    let store = SessionStore::open(dir.path()).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(store.session_exists(&id).unwrap());
    let rows: u32 = store
        .conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, SCHEMA_VERSION);
}

/// Every migration is recorded, in order, with the name it was applied
/// under — the log is what makes a future "which shape is this database
/// in?" answerable without guessing.
#[test]
fn every_applied_migration_is_recorded_by_name_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();

    let mut stmt = store
        .conn
        .prepare("SELECT version, name FROM schema_version ORDER BY version")
        .unwrap();
    let applied: Vec<(u32, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    let expected: Vec<(u32, String)> = MIGRATIONS
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (i as u32 + 1, (*name).to_string()))
        .collect();
    assert_eq!(applied, expected);
}

/// A database from a *newer* smith must be refused rather than written
/// into with this build's assumptions about the tables.
#[test]
fn a_database_from_the_future_is_refused_instead_of_corrupted() {
    let dir = tempfile::tempdir().unwrap();
    SessionStore::open(dir.path()).unwrap();
    {
        let conn = Connection::open(dir.path().join(".smith/sessions.db")).unwrap();
        conn.execute(
                "INSERT INTO schema_version (version, name, applied_at) VALUES (?1, 'from-the-future', 0)",
                params![SCHEMA_VERSION + 5],
            )
            .unwrap();
    }

    let Err(err) = SessionStore::open(dir.path()) else {
        panic!("opening a newer database must fail");
    };
    assert!(
        matches!(err, SessionError::SchemaTooNew { found, supported }
                if found == SCHEMA_VERSION + 5 && supported == SCHEMA_VERSION),
        "{err}"
    );
}

// ---- turn accounting ---------------------------------------------------

fn million_each() -> Usage {
    Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_read: 0,
        cache_write: 0,
    }
}

#[test]
fn a_recorded_turn_round_trips_with_every_token_class() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store
        .create_session("anthropic", "claude-sonnet-5", "/proj")
        .unwrap();

    let turn = TurnRecord {
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        usage: Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read: 3,
            cache_write: 4,
        },
        cost_usd: Some(0.125),
    };
    store.record_turn(&id, &turn).unwrap();

    assert_eq!(store.load_turns(&id).unwrap(), vec![turn]);
    let totals = store.turn_totals(&id).unwrap();
    assert_eq!(totals.turns, 1);
    assert_eq!(totals.usage.cache_write, 4);
    assert_eq!(totals.cost_usd, 0.125);
    assert_eq!(totals.unpriced_turns, 0);
}

/// Acceptance criterion #4, and the reason cost is a stored column rather
/// than a computation.
///
/// The turn below is recorded at the price that was in force when it ran.
/// The test then shows the price table producing a *different* number for
/// the same tokens — and the resumed session still reporting the original.
/// If cost were recomputed on resume, the two asserts could not both hold.
#[test]
fn a_resumed_session_reports_what_it_cost_not_what_it_would_cost_today() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store
        .create_session("anthropic", "claude-sonnet-5", "/proj")
        .unwrap();

    // What the price table said when the turn ran.
    let cost_when_it_ran =
        smith_core::pricing::cost_usd("anthropic", "claude-sonnet-5", &million_each()).unwrap();
    store
        .record_turn(
            &id,
            &TurnRecord {
                provider: "anthropic".into(),
                model: "claude-sonnet-5".into(),
                usage: million_each(),
                cost_usd: Some(cost_when_it_ran),
            },
        )
        .unwrap();
    let before_resume = store.turn_totals(&id).unwrap();
    drop(store);

    // A new process opens the same database — this is what `--resume` does.
    let store = SessionStore::open(dir.path()).unwrap();
    let after_resume = store.turn_totals(&id).unwrap();

    // Identically, not approximately.
    assert_eq!(after_resume, before_resume);
    assert_eq!(after_resume.cost_usd, cost_when_it_ran);

    // Now stand in for the table having moved: the same token counts under
    // a different model's prices give a different answer entirely...
    let repriced =
        smith_core::pricing::cost_usd("anthropic", "claude-opus-5", &million_each()).unwrap();
    assert_ne!(repriced, cost_when_it_ran);
    // ...and the resumed session is unmoved by it, because it never asks.
    assert_eq!(store.turn_totals(&id).unwrap().cost_usd, cost_when_it_ran);
}

/// The sharpest version of the same property: a model this build cannot
/// price *at all* — retired, renamed, or simply never in the table. A
/// recomputing implementation reports nothing here. This one reports the
/// real historical cost.
#[test]
fn cost_survives_a_model_disappearing_from_the_price_table_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store
        .create_session("anthropic", "claude-sonnet-3-retired", "/proj")
        .unwrap();

    // Nothing in this build can produce a price for it.
    assert!(
        smith_core::pricing::cost_usd("anthropic", "claude-sonnet-3-retired", &million_each())
            .is_none()
    );

    store
        .record_turn(
            &id,
            &TurnRecord {
                provider: "anthropic".into(),
                model: "claude-sonnet-3-retired".into(),
                usage: million_each(),
                cost_usd: Some(7.5),
            },
        )
        .unwrap();

    let store = SessionStore::open(dir.path()).unwrap();
    assert_eq!(store.turn_totals(&id).unwrap().cost_usd, 7.5);
}

/// Several turns must sum to the same double across a reopen — "identically"
/// is a claim about floating point, not just about the schema.
#[test]
fn multi_turn_totals_are_bit_identical_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store
        .create_session("anthropic", "claude-sonnet-5", "/proj")
        .unwrap();

    let mut accumulated = 0.0f64;
    for i in 1..=17u32 {
        let usage = Usage {
            input_tokens: i * 977,
            output_tokens: i * 131,
            cache_read: i * 13,
            cache_write: i * 7,
        };
        let cost = smith_core::pricing::cost_usd("anthropic", "claude-sonnet-5", &usage).unwrap();
        accumulated += cost;
        store
            .record_turn(
                &id,
                &TurnRecord {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-5".into(),
                    usage,
                    cost_usd: Some(cost),
                },
            )
            .unwrap();
    }
    let live = store.turn_totals(&id).unwrap();
    drop(store);

    let store = SessionStore::open(dir.path()).unwrap();
    let resumed = store.turn_totals(&id).unwrap();
    assert_eq!(resumed, live);
    assert_eq!(resumed.cost_usd.to_bits(), accumulated.to_bits());
    assert_eq!(resumed.turns, 17);
}

/// An unpriced turn is counted, not silently folded into the total as
/// zero — otherwise a session on a local model looks like it cost nothing
/// *and* like that number is complete.
#[test]
fn unpriced_turns_are_counted_separately_from_the_total() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("ollama", "qwen2.5", "/proj").unwrap();

    for cost in [Some(1.25), None, None] {
        store
            .record_turn(
                &id,
                &TurnRecord {
                    provider: "ollama".into(),
                    model: "qwen2.5".into(),
                    usage: Usage {
                        input_tokens: 100,
                        ..Usage::default()
                    },
                    cost_usd: cost,
                },
            )
            .unwrap();
    }

    let totals = store.turn_totals(&id).unwrap();
    assert_eq!(totals.turns, 3);
    assert_eq!(totals.unpriced_turns, 2);
    assert_eq!(totals.cost_usd, 1.25);
    assert_eq!(totals.usage.input_tokens, 300);
}

#[test]
fn turn_totals_are_scoped_to_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let a = store.create_session("anthropic", "m", "/proj").unwrap();
    let b = store.create_session("anthropic", "m", "/proj").unwrap();

    store
        .record_turn(
            &a,
            &TurnRecord {
                provider: "anthropic".into(),
                model: "m".into(),
                usage: million_each(),
                cost_usd: Some(3.0),
            },
        )
        .unwrap();

    assert_eq!(store.turn_totals(&a).unwrap().cost_usd, 3.0);
    assert_eq!(store.turn_totals(&b).unwrap(), TurnTotals::default());
    assert_eq!(
        store.turn_totals("no-such-session").unwrap(),
        TurnTotals::default()
    );
}

// ---- session management ------------------------------------------------

#[test]
fn list_sessions_is_most_recent_first_and_respects_a_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();

    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = store.create_session("anthropic", "m", "/tmp").unwrap();
        // `updated_at` is millisecond-resolution, so without a nudge all
        // three sort arbitrarily and the assertion below tests nothing.
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .append_message(&id, &Message::user_text("hi"))
            .unwrap();
        ids.push(id);
    }

    let all = store.list_sessions(None).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, ids[2], "newest first");
    assert_eq!(all[2].id, ids[0]);

    assert_eq!(store.list_sessions(Some(2)).unwrap().len(), 2);
}

#[test]
fn deleting_a_session_takes_its_messages_and_turns_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("anthropic", "m", "/tmp").unwrap();
    store
        .append_message(&id, &Message::user_text("hi"))
        .unwrap();
    store
        .record_turn(
            &id,
            &TurnRecord {
                provider: "anthropic".into(),
                model: "m".into(),
                usage: Usage::default(),
                cost_usd: Some(1.0),
            },
        )
        .unwrap();

    assert!(store.delete_session(&id).unwrap());
    assert!(!store.session_exists(&id).unwrap());
    // The cascade only fires with foreign keys switched on, which is off
    // by default in SQLite — without the pragma these rows would survive
    // as orphans and keep counting toward totals.
    assert!(store.load_messages(&id).unwrap().is_empty());
    assert_eq!(store.turn_totals(&id).unwrap().turns, 0);

    // A typo'd id is not a silent success.
    assert!(!store.delete_session("no-such-session").unwrap());
}

#[test]
fn forking_copies_history_up_to_the_cutoff_and_leaves_the_original_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("anthropic", "m", "/tmp").unwrap();
    for text in ["one", "two", "three"] {
        store
            .append_message(&id, &Message::user_text(text))
            .unwrap();
    }

    let last = store.last_seq(&id).unwrap().unwrap();
    assert_eq!(last, 2);

    let forked = store.fork_session(&id, Some(1)).unwrap();
    let branch = store.load_messages(&forked).unwrap();
    assert_eq!(branch.len(), 2, "cutoff is inclusive");
    assert_eq!(branch[0].text(), "one");
    assert_eq!(branch[1].text(), "two");

    // Branching must not disturb what it branched from.
    assert_eq!(store.load_messages(&id).unwrap().len(), 3);
}

#[test]
fn a_fork_does_not_inherit_the_original_spend() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("anthropic", "m", "/tmp").unwrap();
    store
        .append_message(&id, &Message::user_text("hi"))
        .unwrap();
    store
        .record_turn(
            &id,
            &TurnRecord {
                provider: "anthropic".into(),
                model: "m".into(),
                usage: Usage::default(),
                cost_usd: Some(4.20),
            },
        )
        .unwrap();

    let forked = store.fork_session(&id, None).unwrap();
    // Those dollars were spent once. Copying the rows would report them
    // twice and make the project's total a lie.
    assert_eq!(store.turn_totals(&forked).unwrap().cost_usd, 0.0);
    assert_eq!(store.turn_totals(&id).unwrap().cost_usd, 4.20);
}

#[test]
fn forking_an_unknown_session_is_an_error_not_an_empty_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SessionStore::open(dir.path()).unwrap();
    assert!(matches!(
        store.fork_session("no-such-session", None),
        Err(SessionError::NoSuchSession(_))
    ));
}

#[test]
fn last_seq_is_none_for_a_session_with_no_messages() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    let id = store.create_session("anthropic", "m", "/tmp").unwrap();
    assert_eq!(store.last_seq(&id).unwrap(), None);
}
