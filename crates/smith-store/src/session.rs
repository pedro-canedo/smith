use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use smith_core::{ContentBlock, Message, Role, Usage};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("failed to (de)serialize message content: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer smith. Refusing is the safe move:
    /// this build cannot know what a future migration did to the tables it
    /// thinks it understands, and writing into them anyway is how a
    /// downgrade corrupts a session history rather than merely failing to
    /// read it.
    #[error(
        "this project's session database is at schema version {found}, but this build of smith \
         only understands up to {supported} — upgrade smith, or move .smith/sessions.db aside"
    )]
    SchemaTooNew { found: u32, supported: u32 },
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
}

/// One turn's token usage and the cost it incurred **when it ran**.
///
/// `cost_usd` is passed in rather than derived here, and that is the entire
/// point of this type. Prices change; models get renamed and retired. A
/// resumed session that recomputed cost from today's table would report a
/// different total than the session itself ever showed, and a model that has
/// since dropped out of the table would report nothing at all. Storing the
/// dollars alongside the tokens makes the number a historical fact.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// `None` when no price was known at the time — an honest gap, and
    /// distinguishable from a genuinely free turn.
    pub cost_usd: Option<f64>,
}

/// Everything a resumed session needs to carry its running totals forward.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnTotals {
    pub turns: u32,
    pub usage: Usage,
    /// Sum of the per-turn costs recorded at the time. Never a recomputation.
    pub cost_usd: f64,
    /// Turns whose cost could not be computed when they ran, so a frontend can
    /// say "$4.20 + 3 turns of unknown cost" instead of implying $4.20 is all
    /// of it.
    pub unpriced_turns: u32,
}

// ---- schema migrations -----------------------------------------------------

/// One migration: a name for the `schema_version` row, and the function that
/// applies it.
///
/// A `fn` rather than a SQL string because two of the three need to *look* at
/// the database first. The old ad-hoc `ALTER TABLE ... ADD COLUMN goal` swallowed
/// its error to mean "already there", which also silently swallowed a disk
/// error, a locked database, and a typo. Asking `pragma_table_info` is the same
/// idempotency with none of the blindness.
type MigrationFn = fn(&Connection) -> rusqlite::Result<()>;

/// Applied in order; index + 1 is the version each one produces. Append only —
/// editing an existing entry changes what a database that already ran it looks
/// like, without changing its recorded version.
const MIGRATIONS: &[(&str, MigrationFn)] = &[
    ("initial_schema", migrate_initial_schema),
    ("session_goal", migrate_session_goal),
    ("turn_accounting", migrate_turn_accounting),
];

/// The schema version this build produces.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Version 1: sessions and messages as they were originally created.
///
/// Every statement is `IF NOT EXISTS`, which is what makes running this
/// against a pre-versioning database a no-op rather than an error — see
/// `SessionStore::migrate` for why that matters.
fn migrate_initial_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
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
        CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);",
    )
}

/// Version 2: the `goal` column, folded in from the old ad-hoc `ALTER`.
///
/// Some databases in the wild already have this column (the ad-hoc migration
/// added it) and some do not, and both look identical from the outside — which
/// is why this checks instead of guessing.
fn migrate_session_goal(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "sessions", "goal")? {
        return Ok(());
    }
    conn.execute("ALTER TABLE sessions ADD COLUMN goal TEXT", [])?;
    Ok(())
}

/// Version 3: per-turn token and cost accounting.
fn migrate_turn_accounting(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS turns (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id         TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            seq                INTEGER NOT NULL,
            created_at         INTEGER NOT NULL,
            provider           TEXT NOT NULL,
            model              TEXT NOT NULL,
            input_tokens       INTEGER NOT NULL,
            output_tokens      INTEGER NOT NULL,
            cache_read_tokens  INTEGER NOT NULL,
            cache_write_tokens INTEGER NOT NULL,
            -- Nullable on purpose: NULL is 'no price was known when this turn
            -- ran', which is a different fact from 0.0 ('this turn was free').
            cost_usd           REAL
        );
        CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id, seq);",
    )
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    stmt.exists(params![table, column])
}

/// Per-project conversation history, stored at `<project>/.smith/sessions.db`.
/// Global config/secrets live separately in `~/.smith/config.toml`.
pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    pub fn open(project_dir: &Path) -> Result<Self, SessionError> {
        let dir = project_dir.join(".smith");
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("sessions.db"))?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Brings a database up to `SCHEMA_VERSION`, applying only the migrations
    /// it has not already seen.
    ///
    /// **How version 0 is detected: the `schema_version` table does not
    /// exist.** That single rule covers both cases that reach this code with no
    /// version recorded — a brand-new empty file, and a database created by a
    /// smith that predates versioning entirely — and it does not need to tell
    /// them apart, because every migration is written to be idempotent against
    /// objects that are already there (`CREATE TABLE IF NOT EXISTS`, and a
    /// `pragma_table_info` check before the one `ALTER`). So a legacy database
    /// runs migration 1 as a no-op over its existing tables, runs migration 2 as
    /// a no-op if the old ad-hoc `ALTER` already added `goal` (and as a real
    /// `ALTER` if it did not), and only migration 3 actually does anything.
    /// Its rows are never touched.
    ///
    /// The alternative — sniffing `sqlite_master` to guess which historical
    /// shape a database is in — needs a new guess for every future migration
    /// and gets one wrong eventually. Idempotent migrations plus a single
    /// "no table means zero" rule needs none.
    fn migrate(conn: &Connection) -> Result<(), SessionError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version    INTEGER PRIMARY KEY,
                name       TEXT NOT NULL,
                applied_at INTEGER NOT NULL
            );",
        )?;

        let current: u32 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?;

        if current > SCHEMA_VERSION {
            return Err(SessionError::SchemaTooNew {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }

        for (index, (name, migrate)) in MIGRATIONS.iter().enumerate() {
            let version = index as u32 + 1;
            if version <= current {
                continue;
            }
            // One transaction per migration, and SQLite makes DDL
            // transactional, so a crash mid-run leaves the database at the
            // last *fully* applied version rather than half-way through one.
            // Recording the version inside the same transaction is what keeps
            // "the change happened" and "we know it happened" from ever
            // disagreeing.
            conn.execute_batch("BEGIN")?;
            let applied = migrate(conn).and_then(|()| {
                conn.execute(
                    "INSERT INTO schema_version (version, name, applied_at) VALUES (?1, ?2, ?3)",
                    params![version, name, now_millis()],
                )
                .map(|_| ())
            });
            match applied {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    /// The schema version this database is actually at.
    pub fn schema_version(&self) -> Result<u32, SessionError> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .map_err(SessionError::from)
    }

    pub fn create_session(
        &self,
        provider: &str,
        model: &str,
        cwd: &str,
    ) -> Result<String, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.insert_session(&id, provider, model, cwd)?;
        Ok(id)
    }

    /// Ensures a session row exists for `id` (used when the staging session
    /// id was allocated at process start before the first persisted message).
    pub fn ensure_session(
        &self,
        id: &str,
        provider: &str,
        model: &str,
        cwd: &str,
    ) -> Result<(), SessionError> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            self.insert_session(id, provider, model, cwd)?;
        }
        Ok(())
    }

    fn insert_session(
        &self,
        id: &str,
        provider: &str,
        model: &str,
        cwd: &str,
    ) -> Result<(), SessionError> {
        let now = now_millis();
        self.conn.execute(
            "INSERT INTO sessions (id, title, created_at, updated_at, provider, model, cwd) VALUES (?1, NULL, ?2, ?2, ?3, ?4, ?5)",
            params![id, now, provider, model, cwd],
        )?;
        Ok(())
    }

    /// Appends one message and, if the session has no title yet, derives one
    /// from it (first user message, truncated).
    pub fn append_message(&self, session_id: &str, message: &Message) -> Result<(), SessionError> {
        let seq: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content = serde_json::to_string(&message.content)?;
        let now = now_millis();

        self.conn.execute(
            "INSERT INTO messages (session_id, seq, role, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, seq, role, content, now],
        )?;

        if let Some(title) = derive_title(message) {
            self.conn.execute(
                "UPDATE sessions SET updated_at = ?1, title = COALESCE(title, ?2) WHERE id = ?3",
                params![now, title, session_id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now, session_id],
            )?;
        }

        Ok(())
    }

    /// Most recently updated session for this project, if any.
    pub fn latest_session(&self) -> Result<Option<SessionSummary>, SessionError> {
        self.conn
            .query_row(
                "SELECT id, COALESCE(title, '(untitled)'), updated_at FROM sessions ORDER BY updated_at DESC LIMIT 1",
                [],
                |row| {
                    Ok(SessionSummary {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        updated_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(SessionError::from)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        let mut stmt = self
            .conn
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![session_id], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((role, content))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (role, content) = row?;
            let role = if role == "assistant" {
                Role::Assistant
            } else {
                Role::User
            };
            let content: Vec<ContentBlock> = serde_json::from_str(&content)?;
            messages.push(Message { role, content });
        }
        Ok(messages)
    }

    /// Sets (or clears, with `None`) this session's goal. Scoped to a single
    /// session — unlike the old project-wide `.smith/goal.md`, a goal set in
    /// one conversation never bleeds into an unrelated one in the same
    /// project.
    pub fn set_goal(&self, session_id: &str, goal: Option<&str>) -> Result<(), SessionError> {
        self.conn.execute(
            "UPDATE sessions SET goal = ?1 WHERE id = ?2",
            params![goal, session_id],
        )?;
        Ok(())
    }

    pub fn load_goal(&self, session_id: &str) -> Result<Option<String>, SessionError> {
        self.conn
            .query_row(
                "SELECT goal FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(SessionError::from)
    }

    /// Appends one turn's accounting to this session.
    ///
    /// Note what is *not* here: no provider/model price lookup, no reference to
    /// any pricing table. The caller computed `cost_usd` while the turn was
    /// running and this only writes it down. That is the whole mechanism by
    /// which `--resume` reports the same total the session showed at the time.
    pub fn record_turn(&self, session_id: &str, turn: &TurnRecord) -> Result<(), SessionError> {
        let seq: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO turns (
                session_id, seq, created_at, provider, model,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                seq,
                now_millis(),
                turn.provider,
                turn.model,
                turn.usage.input_tokens,
                turn.usage.output_tokens,
                turn.usage.cache_read,
                turn.usage.cache_write,
                turn.cost_usd,
            ],
        )?;
        Ok(())
    }

    /// Everything this session has spent, summed from the recorded turns.
    ///
    /// Summed in `seq` order so a reopened database adds the same doubles in
    /// the same sequence and lands on the same total, bit for bit — floating
    /// point addition is not associative, and "identically" has to mean
    /// identically.
    pub fn turn_totals(&self, session_id: &str) -> Result<TurnTotals, SessionError> {
        let mut stmt = self.conn.prepare(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd
             FROM turns WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                Usage {
                    input_tokens: row.get(0)?,
                    output_tokens: row.get(1)?,
                    cache_read: row.get(2)?,
                    cache_write: row.get(3)?,
                },
                row.get::<_, Option<f64>>(4)?,
            ))
        })?;

        let mut totals = TurnTotals::default();
        for row in rows {
            let (usage, cost) = row?;
            totals.turns += 1;
            totals.usage.add(&usage);
            match cost {
                Some(cost) => totals.cost_usd += cost,
                None => totals.unpriced_turns += 1,
            }
        }
        Ok(totals)
    }

    /// Every recorded turn, oldest first. Not used by the resume path (which
    /// only wants the totals) but the only way to answer "what did that model
    /// actually cost me" after a price change.
    pub fn load_turns(&self, session_id: &str) -> Result<Vec<TurnRecord>, SessionError> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, model, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, cost_usd
             FROM turns WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(TurnRecord {
                provider: row.get(0)?,
                model: row.get(1)?,
                usage: Usage {
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cache_read: row.get(4)?,
                    cache_write: row.get(5)?,
                },
                cost_usd: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SessionError::from)
    }

    /// Whether a session row exists. `load_messages` can't answer this — an
    /// unknown id and a session with no messages both come back as an empty
    /// vector, so `--resume <typo>` would look like a successful resume.
    pub fn session_exists(&self, session_id: &str) -> Result<bool, SessionError> {
        self.conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(SessionError::from)
    }
}

fn derive_title(message: &Message) -> Option<String> {
    if message.role != Role::User {
        return None;
    }
    let text = message.text();
    if text.is_empty() {
        return None;
    }
    let title: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect();
    Some(title)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
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
        assert!(smith_core::pricing::cost_usd(
            "anthropic",
            "claude-sonnet-3-retired",
            &million_each()
        )
        .is_none());

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
            let cost =
                smith_core::pricing::cost_usd("anthropic", "claude-sonnet-5", &usage).unwrap();
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
}
