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
    #[error("no session {0} in this project")]
    NoSuchSession(String),
}

// `Serialize` because the web console's `/api/sessions` answers with the
// summaries verbatim — the row a session list renders from.
#[derive(Debug, Clone, serde::Serialize)]
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
    ("task_snapshots", migrate_task_snapshots),
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

/// Version 4: the task board's stamped snapshot, one row per session.
///
/// A snapshot column rather than one row per task: the board is replaced
/// wholesale on every `write_tasks` call (`TasksUpdated` semantics), and the
/// stamps (`id`, `updated_at`) exist only in the stamped copy — the model's
/// un-stamped `tool_use` input in `messages` cannot reproduce them, which is
/// why resume prefers this table over the legacy history scan.
fn migrate_task_snapshots(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            snapshot   TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    stmt.exists(params![table, column])
}

/// One project's conversation history.
///
/// `open` takes the directory the database lives in and puts `sessions.db`
/// straight into it — it does not know, and must not know, whether that is
/// under the project or under `~/.smith/projects/`. Deciding where a thing
/// lives is `smith-config`'s job (`project_store_dir`); this is the adapter
/// that reads and writes it.
pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    pub fn open(dir: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir)?;
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

    /// Replaces this session's task-board snapshot — the *stamped* copy, ids
    /// and timestamps included, which the model's own `tool_use` input in
    /// `messages` cannot reproduce. Whole-snapshot semantics on purpose: the
    /// board is replaced wholesale on every `write_tasks` call, so a diff
    /// table would model an update that never happens.
    pub fn save_tasks(
        &self,
        session_id: &str,
        tasks: &[smith_core::Task],
    ) -> Result<(), SessionError> {
        let snapshot = serde_json::to_string(tasks)?;
        self.conn.execute(
            "INSERT INTO tasks (session_id, snapshot, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET snapshot = ?2, updated_at = ?3",
            params![session_id, snapshot, now_millis()],
        )?;
        Ok(())
    }

    /// The stamped board, or `None` for a session that never saved one —
    /// the caller falls back to scanning history for the last `write_tasks`
    /// call (the pre-v4 recovery path, kept for old sessions).
    pub fn load_tasks(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<smith_core::Task>>, SessionError> {
        let snapshot: Option<String> = self
            .conn
            .query_row(
                "SELECT snapshot FROM tasks WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        match snapshot {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
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

    /// Sessions in this project, most recently touched first.
    ///
    /// `limit` exists because a long-lived project accumulates hundreds and a
    /// picker only ever shows a screenful; callers wanting all of them pass
    /// `None` and accept the cost.
    pub fn list_sessions(&self, limit: Option<u32>) -> Result<Vec<SessionSummary>, SessionError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, COALESCE(title, '(untitled)'), updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        // SQLite reads a negative LIMIT as "no limit", which is exactly the
        // `None` case and saves a second query shape.
        let rows = stmt.query_map(params![limit.map(i64::from).unwrap_or(-1)], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SessionError::from)
    }

    /// Deletes a session and everything hanging off it. Returns whether a row
    /// was actually removed, so a caller can tell "deleted" from "no such id"
    /// rather than reporting success for a typo.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, SessionError> {
        // `ON DELETE CASCADE` is declared on the child tables, but SQLite
        // ignores it unless foreign keys are switched on per connection — off
        // by default, which would silently orphan every message and turn row.
        self.conn.execute("PRAGMA foreign_keys = ON", [])?;
        let removed = self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        Ok(removed > 0)
    }

    /// Copies a session up to and including `through_seq` into a new one, so a
    /// conversation can branch without losing the original.
    ///
    /// Turn accounting is deliberately **not** copied: those rows record what
    /// was actually spent with a provider, and duplicating them would double
    /// the reported cost of money that was only spent once.
    pub fn fork_session(
        &mut self,
        session_id: &str,
        through_seq: Option<i64>,
    ) -> Result<String, SessionError> {
        let tx = self.conn.transaction()?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();

        let copied = tx.execute(
            "INSERT INTO sessions (id, title, created_at, updated_at, provider, model, cwd, goal)
             SELECT ?1, title, ?2, ?2, provider, model, cwd, goal FROM sessions WHERE id = ?3",
            params![new_id, now, session_id],
        )?;
        if copied == 0 {
            return Err(SessionError::NoSuchSession(session_id.to_string()));
        }

        // -1 means "everything", matching `list_sessions`'s LIMIT convention.
        let cutoff = through_seq.unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO messages (session_id, seq, role, content, created_at)
             SELECT ?1, seq, role, content, created_at
             FROM messages WHERE session_id = ?2 AND seq <= ?3 ORDER BY seq",
            params![new_id, session_id, cutoff],
        )?;

        tx.commit()?;
        Ok(new_id)
    }

    /// Highest message `seq` in a session, or `None` when it has no messages.
    /// A fork's cutoff is meaningless without knowing the range.
    pub fn last_seq(&self, session_id: &str) -> Result<Option<i64>, SessionError> {
        self.conn
            .query_row(
                "SELECT MAX(seq) FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(Option::flatten)
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
mod tests;
