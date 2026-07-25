use crate::provider::{Message, Usage};
use anyhow::{Context, Result};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub struct Database {
    connection: Connection,
    path: PathBuf,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
}

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub total_tokens: i64,
    pub updated_at: i64,
}

pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Default)]
pub struct SessionStats {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub last_input_tokens: Option<i64>,
}

/// Per-model token usage within a session, so switching models can be compared.
pub struct ModelStat {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

pub struct MemoryEntry {
    pub content: String,
}

impl Database {
    pub fn open() -> Result<Self> {
        let data_dir = data_dir()?;
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let path = data_dir.join("kamui.db");
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Self::initialize(connection, path)
    }

    fn initialize(connection: Connection, path: PathBuf) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 provider TEXT NOT NULL,
                 model TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
                 content TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS messages_session_id ON messages(session_id, id);
             CREATE TABLE IF NOT EXISTS usage_records (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 total_tokens INTEGER NOT NULL,
                 finish_reason TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS usage_session_id ON usage_records(session_id, id);",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            connection.execute_batch("PRAGMA user_version = 1;")?;
        }
        if version < 2 {
            connection.execute_batch(
                "ALTER TABLE usage_records
                 ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
                 PRAGMA user_version = 2;",
            )?;
        }
        if version < 3 {
            // Rebuild messages to allow the 'tool' role and store tool-call metadata. SQLite cannot
            // alter a CHECK constraint in place, so the table is recreated and its rows copied.
            connection.execute_batch(
                "ALTER TABLE messages RENAME TO messages_pre_tools;
                 CREATE TABLE messages (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
                     content TEXT NOT NULL,
                     tool_calls TEXT,
                     tool_call_id TEXT,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 INSERT INTO messages (id, session_id, role, content, created_at)
                     SELECT id, session_id, role, content, created_at FROM messages_pre_tools;
                 DROP TABLE messages_pre_tools;
                 CREATE INDEX IF NOT EXISTS messages_session_id ON messages(session_id, id);
                 PRAGMA user_version = 3;",
            )?;
        }
        if version < 4 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 PRAGMA user_version = 4;",
            )?;
        }
        if version < 5 {
            connection.execute_batch(
                "ALTER TABLE usage_records ADD COLUMN model TEXT;
                 PRAGMA user_version = 5;",
            )?;
        }
        if version < 6 {
            // Global, permanent facts the model has been explicitly asked to remember. Not scoped
            // to a session or project — Kamui's database is already one global file (data_dir(),
            // not per-project), so a remembered fact is visible from any project the user opens
            // Kamui in afterward.
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS memory (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     content TEXT NOT NULL,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     updated_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 PRAGMA user_version = 6;",
            )?;
        }
        if version < 7 {
            // `/index`'s semantic-search store: one row per chunk of an indexed file, with its
            // embedding vector (little-endian f32s) and the file's content hash so re-indexing can
            // skip unchanged files. Unlike `memory`, this is project-scoped in spirit but lives in
            // the same global database as everything else (Kamui has one DB file, not one per
            // project) — `path` is project-relative, so chunks from different projects indexed into
            // the same database would collide; this is an accepted v1 limitation (see CLAUDE.md).
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS indexed_files (
                     path TEXT PRIMARY KEY,
                     hash TEXT NOT NULL,
                     indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE TABLE IF NOT EXISTS code_chunks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     path TEXT NOT NULL,
                     start_line INTEGER NOT NULL,
                     end_line INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     embedding BLOB NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS code_chunks_path ON code_chunks(path);
                 PRAGMA user_version = 7;",
            )?;
        }
        Ok(Self { connection, path })
    }

    #[cfg(test)]
    pub fn open_in_memory_for_tests() -> Self {
        Self::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every stored memory entry, oldest first (the order they were learned in).
    pub fn list_memory(&self) -> Result<Vec<MemoryEntry>> {
        let mut statement = self
            .connection
            .prepare("SELECT content FROM memory ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(MemoryEntry {
                content: row.get(0)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Total bytes across all memory content, used to enforce a cap before adding more (see
    /// `tools::remember`); cheaper than loading every row just to sum lengths.
    pub fn total_memory_bytes(&self) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM memory",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Append a new memory entry.
    pub fn remember(&self, content: &str) -> Result<()> {
        self.connection
            .execute("INSERT INTO memory (content) VALUES (?1)", params![content])?;
        Ok(())
    }

    /// Replace the content of an unambiguous entry matched by a case-insensitive substring, so a
    /// superseded fact (e.g. an old preference) can be corrected in place instead of left to
    /// contradict a newer one. Returns `Ok(false)` for no match or an ambiguous (multi-match)
    /// substring, so the caller can ask for something more specific rather than guess.
    pub fn update_memory(&self, substring: &str, new_content: &str) -> Result<bool> {
        let pattern = format!("%{}%", substring.replace('%', "\\%").replace('_', "\\_"));
        let matches: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM memory WHERE content LIKE ?1 ESCAPE '\\'")?
            .query_map([&pattern], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(false);
        }
        self.connection.execute(
            "UPDATE memory SET content = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![matches[0], new_content],
        )?;
        Ok(true)
    }

    /// Delete an unambiguous entry matched by a case-insensitive substring. Same ambiguity
    /// handling as `update_memory`.
    pub fn forget(&self, substring: &str) -> Result<bool> {
        let pattern = format!("%{}%", substring.replace('%', "\\%").replace('_', "\\_"));
        let matches: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM memory WHERE content LIKE ?1 ESCAPE '\\'")?
            .query_map([&pattern], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(false);
        }
        self.connection
            .execute("DELETE FROM memory WHERE id = ?1", [matches[0]])?;
        Ok(true)
    }

    /// Delete every memory entry.
    pub fn clear_memory(&self) -> Result<usize> {
        Ok(self.connection.execute("DELETE FROM memory", [])?)
    }

    pub fn create_session(&self, provider: &str, model: &str) -> Result<Session> {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            title: "New chat".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        self.connection.execute(
            "INSERT INTO sessions (id, title, provider, model) VALUES (?1, ?2, ?3, ?4)",
            params![session.id, session.title, session.provider, session.model],
        )?;
        Ok(session)
    }

    pub fn find_session(&self, id_prefix: &str) -> Result<Option<Session>> {
        let pattern = format!("{id_prefix}%");
        let mut statement = self.connection.prepare(
            "SELECT id, title, provider, model FROM sessions
             WHERE id LIKE ?1 ORDER BY updated_at DESC LIMIT 2",
        )?;
        let sessions = statement
            .query_map([pattern], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if sessions.len() == 1 {
            sessions.into_iter().next()
        } else {
            None
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.title,
                    (SELECT COUNT(*) FROM messages WHERE session_id = s.id),
                    (SELECT COALESCE(SUM(total_tokens), 0)
                     FROM usage_records WHERE session_id = s.id),
                    s.updated_at
             FROM sessions s
             WHERE EXISTS (SELECT 1 FROM messages WHERE session_id = s.id)
             ORDER BY s.updated_at DESC, s.rowid DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                message_count: row.get(2)?,
                total_tokens: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT role, content, tool_calls, tool_call_id
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (role, content, tool_calls, tool_call_id) = row?;
            let mut message = Message::from_parts(&role, content)?;
            if let Some(json) = tool_calls {
                message.tool_calls =
                    serde_json::from_str(&json).context("failed to parse stored tool calls")?;
            }
            message.tool_call_id = tool_call_id;
            Ok(message)
        })
        .collect()
    }

    /// Persist a full turn: every message it produced plus one usage record, atomically. A turn is
    /// usually a user prompt and an assistant answer, but may also include the assistant's tool
    /// requests and the tool results in between.
    pub fn save_turn(
        &self,
        session_id: &str,
        messages: &[Message],
        usage: &Usage,
        model: &str,
        finish_reason: &str,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        for message in messages {
            let tool_calls = if message.tool_calls.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_string(&message.tool_calls)
                        .context("failed to serialize tool calls")?,
                )
            };
            transaction.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    message.role_name(),
                    message.content,
                    tool_calls,
                    message.tool_call_id
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, finish_reason, kind, model)
             VALUES (?1, ?2, ?3, ?4, ?5, 'chat', ?6)",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                finish_reason,
                model
            ],
        )?;
        let title_source = messages
            .iter()
            .find(|message| message.role_name() == "user")
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        transaction.execute(
            "UPDATE sessions SET
                 title = CASE WHEN title = 'New chat' THEN ?2 ELSE title END,
                 updated_at = unixepoch()
             WHERE id = ?1",
            params![session_id, make_title(title_source)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_generated_title(
        &self,
        session_id: &str,
        title: &str,
        usage: &Usage,
        model: &str,
        finish_reason: &str,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE sessions SET title = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, title],
        )?;
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, finish_reason, kind, model)
             VALUES (?1, ?2, ?3, ?4, ?5, 'title', ?6)",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                finish_reason,
                model
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        let mut stats = self.connection.query_row(
            "SELECT COUNT(*) FILTER (WHERE kind = 'chat'), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(total_tokens), 0)
             FROM usage_records WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(SessionStats {
                    request_count: row.get(0)?,
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    total_tokens: row.get(3)?,
                    last_input_tokens: None,
                })
            },
        )?;
        stats.last_input_tokens = self
            .connection
            .query_row(
                "SELECT input_tokens FROM usage_records
                 WHERE session_id = ?1 AND kind = 'chat' ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stats)
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE sessions SET title = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, title],
        )?;
        if changed == 0 {
            anyhow::bail!("session '{session_id}' was not found");
        }
        Ok(())
    }

    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let pattern = format!("%{}%", escape_like(query));
        let mut statement = self.connection.prepare(
            "SELECT m.session_id, s.title, m.role, m.content, m.created_at
             FROM messages m JOIN sessions s ON s.id = m.session_id
             WHERE m.content LIKE ?1 ESCAPE '\\'
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT ?2",
        )?;
        let hits = statement.query_map(params![pattern, limit as i64], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                title: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        hits.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Chat token usage grouped by model for one session, most-used first.
    pub fn model_stats(&self, session_id: &str) -> Result<Vec<ModelStat>> {
        let mut statement = self.connection.prepare(
            "SELECT COALESCE(model, '(unknown)'), COUNT(*),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
             FROM usage_records
             WHERE session_id = ?1 AND kind = 'chat'
             GROUP BY model
             ORDER BY SUM(total_tokens) DESC",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(ModelStat {
                model: row.get(0)?,
                request_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                total_tokens: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    /// A durable key-value store for small UI state, such as the active provider profile.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The content hash `/index` stored for a project-relative path last time it was indexed, or
    /// `None` if the path has never been indexed. Compared against the file's current hash to skip
    /// re-embedding unchanged files.
    pub fn indexed_file_hash(&self, path: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT hash FROM indexed_files WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Record (or update) the hash `/index` last saw for a path.
    pub fn set_indexed_file(&self, path: &str, hash: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO indexed_files (path, hash, indexed_at) VALUES (?1, ?2, unixepoch())
             ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, indexed_at = excluded.indexed_at",
            params![path, hash],
        )?;
        Ok(())
    }

    /// Every project-relative path currently indexed, so `/index` can tell which ones no longer
    /// exist on disk and should be removed.
    pub fn indexed_paths(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare("SELECT path FROM indexed_files")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_indexed_file(&self, path: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM indexed_files WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Drop every chunk previously indexed for a path, before re-chunking it (or because it no
    /// longer exists).
    pub fn delete_chunks_for_path(&self, path: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM code_chunks WHERE path = ?1", [path])?;
        Ok(())
    }

    pub fn insert_chunk(
        &self,
        path: &str,
        start_line: usize,
        end_line: usize,
        content: &str,
        embedding: &[f32],
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO code_chunks (path, start_line, end_line, content, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                path,
                start_line as i64,
                end_line as i64,
                content,
                encode_embedding(embedding)
            ],
        )?;
        Ok(())
    }

    /// The number of chunks currently indexed, for a quick `/index`/status summary without
    /// loading every embedding into memory.
    pub fn chunk_count(&self) -> Result<i64> {
        self.connection
            .query_row("SELECT COUNT(*) FROM code_chunks", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Every indexed chunk, for `search_code` to score against a query embedding. Loaded in full
    /// (no vector index) — a brute-force scan is simple and fast enough at the scale a single
    /// project's chunks reach; see CLAUDE.md for the tradeoff.
    pub fn all_chunks(&self) -> Result<Vec<CodeChunk>> {
        let mut statement = self
            .connection
            .prepare("SELECT path, start_line, end_line, content, embedding FROM code_chunks")?;
        let rows = statement.query_map([], |row| {
            let start_line: i64 = row.get(1)?;
            let end_line: i64 = row.get(2)?;
            let embedding: Vec<u8> = row.get(4)?;
            Ok(CodeChunk {
                path: row.get(0)?,
                start_line: start_line as usize,
                end_line: end_line as usize,
                content: row.get(3)?,
                embedding: decode_embedding(&embedding),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

/// One chunk of an indexed file, as scored by `search_code`.
pub struct CodeChunk {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub embedding: Vec<f32>,
}

/// Encode an embedding vector as little-endian `f32` bytes for the `code_chunks.embedding` BLOB
/// column — simpler than pulling in a serialization crate for a fixed, self-describing layout.
fn encode_embedding(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("chunks_exact(4) yields 4 bytes")))
        .collect()
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KAMUI_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("kamui"))
        .context("could not determine the operating system data directory")
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
    })
}

fn make_title(content: &str) -> String {
    let mut title: String = content.chars().take(40).collect();
    if content.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn database() -> Database {
        Database::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    #[test]
    fn persists_and_reloads_a_tool_turn() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("read it"),
                    Message::tool_request(
                        "",
                        vec![ToolCall {
                            id: "c1".to_string(),
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"a.rs"}"#.to_string(),
                        }],
                    ),
                    Message::tool_result("c1", "fn main() {}"),
                    Message::assistant("It defines main."),
                ],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        let messages = database.load_messages(&session.id).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role_name(), "assistant");
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].name, "read_file");
        assert_eq!(messages[2].role_name(), "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].content, "fn main() {}");
        // The turn counts as a single request despite its extra messages.
        assert_eq!(
            database.session_stats(&session.id).unwrap().request_count,
            1
        );
    }

    #[test]
    fn migration_preserves_messages_and_enables_the_tool_role() {
        let connection = Connection::open_in_memory().unwrap();
        // Reconstruct the pre-tool (user_version 2) schema with one existing message.
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, title TEXT NOT NULL, provider TEXT NOT NULL,
                     model TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 CREATE TABLE messages (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
                     content TEXT NOT NULL,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 CREATE INDEX messages_session_id ON messages(session_id, id);
                 CREATE TABLE usage_records (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                     total_tokens INTEGER NOT NULL, finish_reason TEXT NOT NULL,
                     kind TEXT NOT NULL DEFAULT 'chat',
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 INSERT INTO sessions (id, title, provider, model) VALUES ('s1', 't', 'test', 'm');
                 INSERT INTO messages (session_id, role, content) VALUES ('s1', 'user', 'hi');
                 PRAGMA user_version = 2;",
            )
            .unwrap();

        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();

        // The existing message survives the rebuild.
        let messages = database.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hi");

        // The relaxed CHECK now accepts a tool turn.
        database
            .save_turn(
                "s1",
                &[Message::tool_result("c1", "body")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();
        let messages = database.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role_name(), "tool");
    }

    #[test]
    fn persists_session_messages_and_usage() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("Explain Rust ownership"),
                    Message::assistant("Ownership tracks values."),
                ],
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
                "model",
                "stop",
            )
            .unwrap();

        let messages = database.load_messages(&session.id).unwrap();
        let stats = database.session_stats(&session.id).unwrap();
        let resumed = database.find_session(&session.id).unwrap().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(stats.request_count, 1);
        assert_eq!(stats.total_tokens, 15);
        assert_eq!(stats.last_input_tokens, Some(10));
        assert_eq!(resumed.title, "Explain Rust ownership");
    }

    #[test]
    fn deleting_session_cascades_related_data() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        database.delete_session(&session.id).unwrap();

        assert!(database.find_session(&session.id).unwrap().is_none());
        assert!(database.load_messages(&session.id).unwrap().is_empty());
    }

    #[test]
    fn session_summary_does_not_multiply_usage_by_message_count() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                },
                "model",
                "stop",
            )
            .unwrap();

        let summaries = database.list_sessions().unwrap();

        assert_eq!(summaries[0].message_count, 2);
        assert_eq!(summaries[0].total_tokens, 6);
    }

    #[test]
    fn renames_session_and_updates_summary() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        database
            .rename_session(&session.id, "Custom title")
            .unwrap();

        let resumed = database.find_session(&session.id).unwrap().unwrap();
        assert_eq!(resumed.title, "Custom title");
        assert_eq!(database.list_sessions().unwrap()[0].title, "Custom title");
    }

    #[test]
    fn renaming_missing_session_is_an_error() {
        let database = database();
        assert!(database.rename_session("missing", "title").is_err());
    }

    #[test]
    fn search_matches_message_content_and_ignores_wildcards() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("How does ownership work in Rust"),
                    Message::assistant("Ownership tracks each value's owner."),
                ],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        let hits = database.search_messages("ownership", 20).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.session_id == session.id));

        // A literal percent must not behave as a wildcard.
        assert!(database.search_messages("%", 20).unwrap().is_empty());
    }

    #[test]
    fn model_stats_break_down_usage_by_model() {
        let database = database();
        let session = database.create_session("test", "sol").unwrap();
        let turn = |model: &str, total: u64| {
            database
                .save_turn(
                    &session.id,
                    &[Message::user("hi"), Message::assistant("yo")],
                    &Usage {
                        prompt_tokens: total,
                        completion_tokens: 0,
                        total_tokens: total,
                    },
                    model,
                    "stop",
                )
                .unwrap();
        };
        turn("gpt-5.6-sol", 15);
        turn("codeqwen:latest", 6);
        turn("gpt-5.6-sol", 4);

        let stats = database.model_stats(&session.id).unwrap();

        // Two distinct models, ordered by total tokens descending.
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].model, "gpt-5.6-sol");
        assert_eq!(stats[0].request_count, 2);
        assert_eq!(stats[0].total_tokens, 19);
        assert_eq!(stats[1].model, "codeqwen:latest");
        assert_eq!(stats[1].request_count, 1);
        assert_eq!(stats[1].total_tokens, 6);
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let database = database();
        assert_eq!(database.get_setting("active_profile").unwrap(), None);

        database.set_setting("active_profile", "ollama").unwrap();
        assert_eq!(
            database.get_setting("active_profile").unwrap().as_deref(),
            Some("ollama")
        );

        database.set_setting("active_profile", "openai").unwrap();
        assert_eq!(
            database.get_setting("active_profile").unwrap().as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn session_list_hides_empty_sessions() {
        let database = database();
        database.create_session("test", "model").unwrap();

        assert!(database.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn remember_and_list_memory_round_trip_in_insertion_order() {
        let database = database();
        database.remember("Prefers bun over node.").unwrap();
        database.remember("Prefers uv over pip.").unwrap();

        let entries = database.list_memory().unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "Prefers bun over node.");
        assert_eq!(entries[1].content, "Prefers uv over pip.");
    }

    #[test]
    fn total_memory_bytes_sums_all_entries() {
        let database = database();
        database.remember("abc").unwrap();
        database.remember("de").unwrap();

        assert_eq!(database.total_memory_bytes().unwrap(), 5);
    }

    #[test]
    fn update_memory_replaces_an_unambiguous_match() {
        let database = database();
        database.remember("Prefers node over bun.").unwrap();

        let updated = database
            .update_memory("node over bun", "bun over node.")
            .unwrap();

        assert!(updated);
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "bun over node.");
    }

    #[test]
    fn update_memory_fails_on_no_match_or_ambiguous_match() {
        let database = database();
        database.remember("Prefers bun.").unwrap();
        database.remember("Prefers uv.").unwrap();

        assert!(!database.update_memory("nonexistent", "x").unwrap());
        // Both entries contain "Prefers", so this is ambiguous.
        assert!(!database.update_memory("Prefers", "x").unwrap());
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn forget_removes_an_unambiguous_match() {
        let database = database();
        database.remember("Prefers bun over node.").unwrap();
        database.remember("Prefers uv over pip.").unwrap();

        assert!(database.forget("bun").unwrap());

        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Prefers uv over pip.");
    }

    #[test]
    fn forget_fails_on_no_match_or_ambiguous_match() {
        let database = database();
        database.remember("Prefers bun.").unwrap();
        database.remember("Prefers uv.").unwrap();

        assert!(!database.forget("nonexistent").unwrap());
        assert!(!database.forget("Prefers").unwrap());
        assert_eq!(database.list_memory().unwrap().len(), 2);
    }

    #[test]
    fn clear_memory_removes_every_entry_and_reports_the_count() {
        let database = database();
        database.remember("one").unwrap();
        database.remember("two").unwrap();

        assert_eq!(database.clear_memory().unwrap(), 2);
        assert!(database.list_memory().unwrap().is_empty());
    }

    #[test]
    fn memory_matching_is_case_insensitive_and_escapes_like_wildcards() {
        let database = database();
        database.remember("100% sure about this_thing").unwrap();

        // '%' and '_' are SQL LIKE wildcards; a literal search for them must not match everything.
        assert!(!database.forget("50%").unwrap());
        assert!(database.forget("100% SURE").unwrap());
    }

    #[test]
    fn embedding_round_trips_through_the_blob_column() {
        let database = database();
        let vector = vec![0.5_f32, -1.25, 0.0, 3.0];
        database
            .insert_chunk("src/main.rs", 1, 10, "fn main() {}", &vector)
            .unwrap();

        let chunks = database.all_chunks().unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].path, "src/main.rs");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 10);
        assert_eq!(chunks[0].content, "fn main() {}");
        assert_eq!(chunks[0].embedding, vector);
    }

    #[test]
    fn indexed_file_hash_tracks_the_most_recent_hash() {
        let database = database();
        assert_eq!(database.indexed_file_hash("a.rs").unwrap(), None);

        database.set_indexed_file("a.rs", "hash1").unwrap();
        assert_eq!(
            database.indexed_file_hash("a.rs").unwrap(),
            Some("hash1".to_string())
        );

        database.set_indexed_file("a.rs", "hash2").unwrap();
        assert_eq!(
            database.indexed_file_hash("a.rs").unwrap(),
            Some("hash2".to_string())
        );
    }

    #[test]
    fn deleting_a_path_removes_its_chunks_and_index_entry() {
        let database = database();
        database.set_indexed_file("a.rs", "hash1").unwrap();
        database
            .insert_chunk("a.rs", 1, 5, "content", &[0.1])
            .unwrap();

        database.delete_chunks_for_path("a.rs").unwrap();
        database.delete_indexed_file("a.rs").unwrap();

        assert!(database.all_chunks().unwrap().is_empty());
        assert_eq!(database.indexed_file_hash("a.rs").unwrap(), None);
        assert!(database.indexed_paths().unwrap().is_empty());
    }

    #[test]
    fn chunk_count_reflects_inserted_chunks() {
        let database = database();
        assert_eq!(database.chunk_count().unwrap(), 0);
        database.insert_chunk("a.rs", 1, 5, "x", &[0.1]).unwrap();
        database.insert_chunk("a.rs", 6, 10, "y", &[0.2]).unwrap();
        assert_eq!(database.chunk_count().unwrap(), 2);
    }
}
