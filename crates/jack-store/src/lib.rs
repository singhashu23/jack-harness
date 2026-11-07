//! Per-session event store for jack.
//!
//! One SQLite file per session, written only by that session's process, so
//! sessions never contend on locks (design doc, Problem 3). Holds:
//! - `blobs`: full tool outputs and file snapshots, addressed by content hash
//! - `blobs_fts`: FTS5 index over blobs, for `recall` search
//! - `reads`: what each file looked like the last time it was shown to the model
//! - `events`: one row per tool call, with raw vs. sent byte counts

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;

/// How much of a file the model last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The whole file (or a diff applied on top of a whole-file view).
    Full,
    /// Only the outline.
    Outline,
    /// A symbol or line range.
    Part,
}

impl View {
    fn as_str(self) -> &'static str {
        match self {
            View::Full => "full",
            View::Outline => "outline",
            View::Part => "part",
        }
    }
    fn parse(s: &str) -> View {
        match s {
            "full" => View::Full,
            "outline" => View::Outline,
            _ => View::Part,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadRecord {
    pub hash: String,
    pub handle: String,
    pub seq: i64,
    pub view: View,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub handle: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub calls: i64,
    pub raw_bytes: i64,
    pub sent_bytes: i64,
}

/// One content block of a recorded agent transcript.
#[derive(Debug, Clone, Default)]
pub struct Msg {
    pub uuid: String,
    pub idx: usize,
    pub ts: Option<String>,
    /// "main", or the subagent transcript it came from.
    pub source: String,
    /// user, user_meta, assistant, thinking, tool_use, tool_result
    pub kind: String,
    pub tool: Option<String>,
    pub tool_id: Option<String>,
    pub handle: Option<String>,
    pub bytes: usize,
    pub is_error: bool,
}

/// Token usage of one model response.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub msg_id: String,
    pub ts: Option<String>,
    pub source: String,
    pub model: Option<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

pub struct Store {
    conn: Connection,
}

/// Short content hash used as a handle: first 16 hex chars of BLAKE3.
pub fn handle_for(content: &[u8]) -> String {
    blake3::hash(content).to_hex()[..16].to_string()
}

/// Escape free text into an FTS5 query: every token quoted, ANDed together,
/// so user input can never be parsed as FTS syntax.
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// Open another session's store without writing to it (no schema
    /// setup, no pragmas), so the one-writer-per-file rule holds while that
    /// session may still be running.
    pub fn open_readonly(path: &Path) -> Result<Store> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Store { conn })
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS blobs(
                 handle  TEXT PRIMARY KEY,
                 content TEXT NOT NULL,
                 bytes   INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS blobs_fts
                 USING fts5(handle UNINDEXED, content);
             CREATE TABLE IF NOT EXISTS reads(
                 path   TEXT PRIMARY KEY,
                 hash   TEXT NOT NULL,
                 handle TEXT NOT NULL,
                 seq    INTEGER NOT NULL,
                 view   TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events(
                 id         INTEGER PRIMARY KEY,
                 ts         INTEGER NOT NULL DEFAULT (unixepoch()),
                 kind       TEXT NOT NULL,
                 tool       TEXT,
                 payload    TEXT,
                 raw_bytes  INTEGER NOT NULL DEFAULT 0,
                 sent_bytes INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS meta(
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS msgs(
                 uuid     TEXT NOT NULL,
                 idx      INTEGER NOT NULL,
                 ts       TEXT,
                 source   TEXT NOT NULL,
                 kind     TEXT NOT NULL,
                 tool     TEXT,
                 tool_id  TEXT,
                 handle   TEXT,
                 bytes    INTEGER NOT NULL DEFAULT 0,
                 is_error INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(uuid, idx));
             CREATE INDEX IF NOT EXISTS msgs_tool_id ON msgs(tool_id);
             CREATE TABLE IF NOT EXISTS usage(
                 msg_id      TEXT PRIMARY KEY,
                 ts          TEXT,
                 source      TEXT NOT NULL,
                 model       TEXT,
                 input       INTEGER NOT NULL,
                 output      INTEGER NOT NULL,
                 cache_read  INTEGER NOT NULL,
                 cache_write INTEGER NOT NULL);",
        )?;
        Ok(Store { conn })
    }

    /// Store content; returns its handle. Identical content is stored once.
    pub fn put_blob(&self, content: &str) -> Result<String> {
        let handle = handle_for(content.as_bytes());
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO blobs(handle, content, bytes) VALUES (?1, ?2, ?3)",
            params![handle, content, content.len() as i64],
        )?;
        if inserted > 0 {
            self.conn.execute(
                "INSERT INTO blobs_fts(handle, content) VALUES (?1, ?2)",
                params![handle, content],
            )?;
        }
        Ok(handle)
    }

    pub fn get_blob(&self, handle: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT content FROM blobs WHERE handle = ?1", [handle], |r| r.get(0))
            .optional()?)
    }

    /// Keyword search over everything stored this session (BM25-ranked).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        let mut stmt = self.conn.prepare(
            "SELECT handle, snippet(blobs_fts, 1, '[', ']', ' … ', 12)
             FROM blobs_fts WHERE blobs_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![q, limit as i64], |r| {
            Ok(Hit { handle: r.get(0)?, snippet: r.get(1)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn last_read(&self, path: &str) -> Result<Option<ReadRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT hash, handle, seq, view FROM reads WHERE path = ?1",
                [path],
                |r| {
                    Ok(ReadRecord {
                        hash: r.get(0)?,
                        handle: r.get(1)?,
                        seq: r.get(2)?,
                        view: View::parse(&r.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?)
    }

    /// Record that `path` (with contents stored under `handle`) was shown to
    /// the model. Returns the read sequence number.
    pub fn record_read(&self, path: &str, hash: &str, handle: &str, view: View) -> Result<i64> {
        let seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM reads", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO reads(path, hash, handle, seq, view) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET hash=excluded.hash, handle=excluded.handle,
                                             seq=excluded.seq, view=excluded.view",
            params![path, hash, handle, seq, view.as_str()],
        )?;
        Ok(seq)
    }

    pub fn log_event(
        &self,
        kind: &str,
        tool: Option<&str>,
        payload: &serde_json::Value,
        raw_bytes: usize,
        sent_bytes: usize,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events(kind, tool, payload, raw_bytes, sent_bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, tool, payload.to_string(), raw_bytes as i64, sent_bytes as i64],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Run `f` in one transaction (much faster for bulk ingest).
    pub fn in_tx<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Record one transcript content block. Idempotent on (uuid, idx).
    /// Returns true if it was new.
    pub fn insert_msg(&self, m: &Msg) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO msgs(uuid, idx, ts, source, kind, tool, tool_id, handle, bytes, is_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![m.uuid, m.idx as i64, m.ts, m.source, m.kind, m.tool, m.tool_id, m.handle, m.bytes as i64, m.is_error],
        )?;
        Ok(n > 0)
    }

    /// Name of the tool a `tool_use` id belongs to, if recorded.
    pub fn tool_for_id(&self, tool_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT tool FROM msgs WHERE tool_id = ?1 AND kind = 'tool_use' LIMIT 1",
                [tool_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Record (or update) token usage for one API response. A response is
    /// split over several transcript entries that all repeat its usage, so
    /// this keys on the message id instead of summing entries.
    pub fn upsert_usage(&self, u: &Usage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO usage(msg_id, ts, source, model, input, output, cache_read, cache_write)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(msg_id) DO UPDATE SET ts=excluded.ts, model=excluded.model,
                 input=excluded.input, output=excluded.output,
                 cache_read=excluded.cache_read, cache_write=excluded.cache_write",
            params![u.msg_id, u.ts, u.source, u.model, u.input, u.output, u.cache_read, u.cache_write],
        )?;
        Ok(())
    }

    /// Token totals per model.
    pub fn usage_by_model(&self) -> Result<Vec<(String, Usage)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(model, '?'), COUNT(*), SUM(input), SUM(output), SUM(cache_read), SUM(cache_write)
             FROM usage GROUP BY 1 ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Usage {
                    msg_id: format!("{} responses", r.get::<_, i64>(1)?),
                    input: r.get(2)?,
                    output: r.get(3)?,
                    cache_read: r.get(4)?,
                    cache_write: r.get(5)?,
                    ..Default::default()
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Count of recorded blocks per kind (user, assistant, tool_use, ...).
    pub fn msg_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT kind, COUNT(*) FROM msgs GROUP BY 1 ORDER BY 2 DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Per tool: calls and total bytes of results returned into context.
    pub fn tool_result_bytes(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(tool, '?'), COUNT(*), SUM(bytes) FROM msgs
             WHERE kind = 'tool_result' GROUP BY 1 ORDER BY 3 DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Per-tool totals of raw bytes (what an unshaped tool would have sent)
    /// vs. bytes actually sent to the model.
    /// Payloads of all events of one kind, oldest first: (unix time, payload).
    pub fn events_of_kind(&self, kind: &str) -> Result<Vec<(i64, serde_json::Value)>> {
        let mut stmt = self.conn.prepare("SELECT ts, payload FROM events WHERE kind = ?1 ORDER BY id")?;
        let rows = stmt.query_map([kind], |r| {
            let p: Option<String> = r.get(1)?;
            Ok((r.get(0)?, p.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default()))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Text of the first recorded block of a kind (e.g. a session's first prompt).
    pub fn first_text(&self, kind: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT b.content FROM msgs m JOIN blobs b ON b.handle = m.handle
                 WHERE m.kind = ?1 ORDER BY m.ts, m.rowid LIMIT 1",
                [kind],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn stats(&self) -> Result<Vec<(String, Stats)>> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(tool, kind), COUNT(*), SUM(raw_bytes), SUM(sent_bytes)
             FROM events WHERE kind = 'tool_call' GROUP BY 1 ORDER BY 3 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Stats { calls: r.get(1)?, raw_bytes: r.get(2)?, sent_bytes: r.get(3)? },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_dedupe_and_search() {
        let s = Store::open_in_memory().unwrap();
        let h1 = s.put_blob("fn verify_token() {}").unwrap();
        let h2 = s.put_blob("fn verify_token() {}").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(s.get_blob(&h1).unwrap().unwrap(), "fn verify_token() {}");
        let hits = s.search("verify_token", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].handle, h1);
        // FTS syntax in user input must not error.
        assert!(s.search("foo AND \"bar OR (", 5).is_ok());
    }

    #[test]
    fn reads_track_latest_view() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.last_read("a.rs").unwrap().is_none());
        let a = s.record_read("a.rs", "h1", "b1", View::Outline).unwrap();
        let b = s.record_read("a.rs", "h2", "b2", View::Full).unwrap();
        assert!(b > a);
        let r = s.last_read("a.rs").unwrap().unwrap();
        assert_eq!((r.hash.as_str(), r.view), ("h2", View::Full));
    }

    #[test]
    fn transcript_rows_are_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let m = Msg { uuid: "u1".into(), kind: "tool_use".into(), tool: Some("Read".into()), tool_id: Some("t1".into()), source: "main".into(), ..Default::default() };
        assert!(s.insert_msg(&m).unwrap());
        assert!(!s.insert_msg(&m).unwrap());
        assert_eq!(s.tool_for_id("t1").unwrap().as_deref(), Some("Read"));
        let u = Usage { msg_id: "m1".into(), source: "main".into(), model: Some("x".into()), input: 1, output: 2, cache_read: 3, cache_write: 4, ..Default::default() };
        s.upsert_usage(&u).unwrap();
        s.upsert_usage(&u).unwrap();
        let by = s.usage_by_model().unwrap();
        assert_eq!((by[0].1.input, by[0].1.output, by[0].1.cache_read), (1, 2, 3));
        s.set_meta("k", "1").unwrap();
        s.set_meta("k", "2").unwrap();
        assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn readonly_open_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.db");
        {
            let s = Store::open(&p).unwrap();
            s.log_event("checkpoint", None, &serde_json::json!({"note": "n1"}), 0, 0).unwrap();
        }
        let before = std::fs::read(&p).unwrap();
        let ro = Store::open_readonly(&p).unwrap();
        let ev = ro.events_of_kind("checkpoint").unwrap();
        assert_eq!(ev[0].1["note"], "n1");
        assert!(ro.log_event("x", None, &serde_json::json!({}), 0, 0).is_err(), "read-only store must refuse writes");
        drop(ro);
        assert_eq!(before, std::fs::read(&p).unwrap());
    }

    #[test]
    fn stats_sum_bytes() {
        let s = Store::open_in_memory().unwrap();
        let p = serde_json::json!({});
        s.log_event("tool_call", Some("jack_read"), &p, 1000, 100).unwrap();
        s.log_event("tool_call", Some("jack_read"), &p, 500, 50).unwrap();
        let st = s.stats().unwrap();
        assert_eq!(st[0].0, "jack_read");
        assert_eq!((st[0].1.calls, st[0].1.raw_bytes, st[0].1.sent_bytes), (2, 1500, 150));
    }
}
