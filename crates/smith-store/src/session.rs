use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use smith_core::{ContentBlock, Message, Role};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("failed to (de)serialize message content: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,
    title      TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    cwd        TEXT NOT NULL,
    goal       TEXT
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
        conn.execute_batch(SCHEMA)?;
        // Lightweight migration for DBs created before `goal` existed —
        // sqlite errors on a duplicate column, which just means it's
        // already there.
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN goal TEXT", []);
        Ok(Self { conn })
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
}
