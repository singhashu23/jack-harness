//! Per-session event store for jack.
//!
//! One SQLite file per session, written only by that session's process, so
//! sessions never contend on locks (design doc, Problem 3). Holds:
//! - `blobs`: full tool outputs and file snapshots, addressed by content hash
//! - `blobs_fts`: FTS5 index over blobs, for `recall` search
//! - `reads`: what each file looked like the last time it was shown to the model
//! - `events`: one row per tool call, with raw vs. sent byte counts

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
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
                 sent_bytes INTEGER NOT NULL DEFAULT 0);",
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

    /// Per-tool totals of raw bytes (what an unshaped tool would have sent)
    /// vs. bytes actually sent to the model.
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
