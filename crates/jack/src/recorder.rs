//! Recorder: copies Claude Code transcripts into jack session stores
//! (build step 1). Run from Claude Code hooks (Stop, PreCompact, SessionEnd)
//! as `jack hook record`, or in bulk with `jack import`.
//!
//! Each Claude session gets its own store, `sessions/cc-<session id>`, so the
//! one-writer-per-file rule holds. Ingest is incremental (a byte offset per
//! transcript file) and idempotent (rows keyed by entry uuid + block index),
//! so running it again never duplicates anything.

use anyhow::{Context, Result};
use jack_store::{Msg, Store, Usage};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Text of a tool_result's content: a string, or a list of blocks.
fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some(t) => format!("[{t}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Ingest one transcript entry. Returns how many new blocks were recorded.
fn ingest_entry(store: &Store, d: &Value, source: &str) -> Result<usize> {
    let typ = d["type"].as_str().unwrap_or("");
    if typ != "user" && typ != "assistant" {
        return Ok(0);
    }
    let Some(uuid) = d["uuid"].as_str() else { return Ok(0) };
    let ts = d["timestamp"].as_str().map(str::to_string);
    let msg = &d["message"];
    let mut new = 0;
    let mut push = |idx: usize, kind: &str, text: &str, tool: Option<String>, tool_id: Option<String>, is_error: bool| -> Result<()> {
        let handle = if text.is_empty() { None } else { Some(store.put_blob(text)?) };
        let m = Msg {
            uuid: uuid.to_string(),
            idx,
            ts: ts.clone(),
            source: source.to_string(),
            kind: kind.to_string(),
            tool,
            tool_id,
            handle,
            bytes: text.len(),
            is_error,
        };
        if store.insert_msg(&m)? {
            new += 1;
        }
        Ok(())
    };

    if typ == "user" {
        // isMeta entries are injected by the harness (reminders, command output).
        let kind = if d["isMeta"].as_bool().unwrap_or(false) { "user_meta" } else { "user" };
        match &msg["content"] {
            Value::String(s) => push(0, kind, s, None, None, false)?,
            Value::Array(blocks) => {
                for (i, b) in blocks.iter().enumerate() {
                    match b["type"].as_str() {
                        Some("text") => push(i, kind, b["text"].as_str().unwrap_or(""), None, None, false)?,
                        Some("tool_result") => {
                            let id = b["tool_use_id"].as_str().map(str::to_string);
                            let tool = match &id {
                                Some(id) => store.tool_for_id(id)?,
                                None => None,
                            };
                            let text = result_text(&b["content"]);
                            push(i, "tool_result", &text, tool, id, b["is_error"].as_bool().unwrap_or(false))?;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        return Ok(new);
    }

    // Assistant: one API response is split over several entries that all
    // repeat its usage, so usage is keyed on the response id.
    if let (Some(id), Some(u)) = (msg["id"].as_str(), msg.get("usage").filter(|u| u.is_object())) {
        store.upsert_usage(&Usage {
            msg_id: id.to_string(),
            ts: ts.clone(),
            source: source.to_string(),
            model: msg["model"].as_str().map(str::to_string),
            input: u["input_tokens"].as_i64().unwrap_or(0),
            output: u["output_tokens"].as_i64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_i64().unwrap_or(0),
            cache_write: u["cache_creation_input_tokens"].as_i64().unwrap_or(0),
        })?;
    }
    if let Some(blocks) = msg["content"].as_array() {
        for (i, b) in blocks.iter().enumerate() {
            match b["type"].as_str() {
                Some("text") => push(i, "assistant", b["text"].as_str().unwrap_or(""), None, None, false)?,
                Some("thinking") => push(i, "thinking", b["thinking"].as_str().unwrap_or(""), None, None, false)?,
                Some("tool_use") => push(
                    i,
                    "tool_use",
                    &b["input"].to_string(),
                    b["name"].as_str().map(str::to_string),
                    b["id"].as_str().map(str::to_string),
                    false,
                )?,
                _ => {}
            }
        }
    }
    Ok(new)
}

/// Ingest the new complete lines of one transcript file.
pub fn ingest_file(store: &Store, path: &Path, source: &str) -> Result<usize> {
    let key = format!("offset:{}", path.display());
    let len = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?.len();
    let mut off: u64 = store.get_meta(&key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    if off > len {
        off = 0; // file was rewritten; rows are idempotent, so re-reading is safe
    }
    if off == len {
        return Ok(0);
    }
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(off))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    // Only whole lines: the last one may still be being written.
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else { return Ok(0) };
    store.in_tx(|s| {
        let mut n = 0;
        let mut cwd: Option<String> = None;
        for line in buf[..last_nl].split(|&b| b == b'\n') {
            let Ok(d) = serde_json::from_slice::<Value>(line) else { continue };
            if cwd.is_none() {
                cwd = d["cwd"].as_str().map(str::to_string);
            }
            n += ingest_entry(s, &d, source)?;
        }
        if source == "main"
            && let Some(c) = cwd
        {
            s.set_meta("cwd", &c)?;
        }
        s.set_meta(&key, &(off + last_nl as u64 + 1).to_string())?;
        Ok(n)
    })
}

pub fn store_path(home: &Path, session_id: &str) -> PathBuf {
    home.join("sessions").join(format!("cc-{session_id}")).join("store.db")
}

/// Ingest a session's main transcript and its subagent transcripts.
pub fn ingest_session(home: &Path, transcript: &Path, session_id: &str) -> Result<usize> {
    let store = Store::open(&store_path(home, session_id))?;
    store.set_meta("transcript", &transcript.display().to_string())?;
    let mut n = ingest_file(&store, transcript, "main")?;
    let sub = transcript.with_extension("").join("subagents");
    if let Ok(rd) = std::fs::read_dir(&sub) {
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        for f in files {
            let src = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            n += ingest_file(&store, &f, &src)?;
        }
    }
    Ok(n)
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

/// `jack hook record`: called by Claude Code hooks with a JSON payload on
/// stdin. Never blocks the session and never fails it: errors go to
/// `<jack home>/hook.log` and the exit code is always 0.
pub fn hook_record(home: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let run = || -> Result<usize> {
        let p: Value = serde_json::from_str(&input).context("hook payload is not JSON")?;
        let t = expand_home(p["transcript_path"].as_str().context("payload has no transcript_path")?);
        let sid = match p["session_id"].as_str() {
            Some(s) => s.to_string(),
            None => t.file_stem().context("no session id")?.to_string_lossy().into_owned(),
        };
        ingest_session(home, &t, &sid)
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    let err = match res {
        Ok(Ok(_)) => return,
        Ok(Err(e)) => format!("{e:#}"),
        Err(_) => "panic during ingest".to_string(),
    };
    let _ = std::fs::create_dir_all(home);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("hook.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{t} hook record: {err}");
    }
}

/// All top-level Claude Code transcripts (`~/.claude/projects/*/*.jsonl`).
pub fn all_transcripts() -> Vec<PathBuf> {
    let root = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects");
    let mut out = Vec::new();
    for proj in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "jsonl") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// $ per million tokens: (input, output, cache read, cache write).
type Price = (f64, f64, f64, f64);

/// Approximate API prices. Cache writes are priced at the 5-minute rate.
/// Unknown models return None.
pub fn price(model: &str) -> Option<Price> {
    let table: &[(&str, Price)] = &[
        ("claude-opus-5-5", (4.0, 20.0, 0.20, 5.0)),
        ("claude-opus-5", (5.0, 25.0, 0.50, 6.25)),
        ("claude-sonnet-5-5", (2.0, 10.0, 0.20, 2.5)),
        ("claude-haiku-5-5", (0.10, 0.50, 0.01, 0.125)),
        ("claude-fable-5-1", (10.0, 50.0, 0.25, 12.5)),
    ];
    table.iter().find(|(m, _)| model.starts_with(m)).map(|(_, p)| *p)
}

pub fn cost(model: &str, u: &Usage) -> Option<f64> {
    let (i, o, r, w) = price(model)?;
    Some((u.input as f64 * i + u.output as f64 * o + u.cache_read as f64 * r + u.cache_write as f64 * w) / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    fn sample() -> String {
        use serde_json::json;
        let mut s = String::new();
        s += &line(json!({"type": "user", "uuid": "u1", "cwd": "/proj", "message": {"role": "user", "content": "fix the bug"}}));
        // One response split over two entries, both repeating its usage.
        let usage = json!({"input_tokens": 10, "output_tokens": 50, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 200});
        s += &line(json!({"type": "assistant", "uuid": "a1", "message": {"id": "m1", "model": "claude-opus-5-5", "usage": usage,
            "content": [{"type": "text", "text": "Reading it."}]}}));
        s += &line(json!({"type": "assistant", "uuid": "a2", "message": {"id": "m1", "model": "claude-opus-5-5", "usage": usage,
            "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/proj/a.rs"}}]}}));
        s += &line(json!({"type": "user", "uuid": "u2", "message": {"role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}\n".repeat(100)}]}}));
        s += &line(json!({"type": "system", "uuid": "s1"}));
        s
    }

    #[test]
    fn ingest_is_incremental_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("sess.jsonl");
        let mut body = sample();
        // A half-written last line must be left for the next run.
        body += "{\"type\": \"user\", \"uuid\": \"u3\"";
        std::fs::write(&t, &body).unwrap();
        let home = dir.path().join("home");
        assert_eq!(ingest_session(&home, &t, "s").unwrap(), 4);
        assert_eq!(ingest_session(&home, &t, "s").unwrap(), 0);

        let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
        f.write_all(b", \"message\": {\"content\": \"thanks\"}}\n").unwrap();
        assert_eq!(ingest_session(&home, &t, "s").unwrap(), 1);

        let store = Store::open(&store_path(&home, "s")).unwrap();
        let by = store.usage_by_model().unwrap();
        assert_eq!(by.len(), 1);
        assert_eq!((by[0].1.input, by[0].1.output, by[0].1.cache_read), (10, 50, 1000), "usage counted once");
        let tools = store.tool_result_bytes().unwrap();
        assert_eq!(tools[0].0, "Read");
        assert_eq!(tools[0].2, 1300);
        assert_eq!(store.get_meta("cwd").unwrap().as_deref(), Some("/proj"));
        let c = cost("claude-opus-5-5", &by[0].1).unwrap();
        assert!((c - (10.0 * 4.0 + 50.0 * 20.0 + 1000.0 * 0.2 + 200.0 * 5.0) / 1e6).abs() < 1e-12);
    }

    #[test]
    fn subagent_transcripts_are_included() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("sess.jsonl");
        std::fs::write(&t, sample()).unwrap();
        let sub = dir.path().join("sess/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-x.jsonl"), sample().replace("\"u1\"", "\"x1\"").replace("\"a1\"", "\"x2\"").replace("\"a2\"", "\"x3\"").replace("\"u2\"", "\"x4\"").replace("\"m1\"", "\"m2\"")).unwrap();
        let home = dir.path().join("home");
        assert_eq!(ingest_session(&home, &t, "s").unwrap(), 8);
        let store = Store::open(&store_path(&home, "s")).unwrap();
        let by = store.usage_by_model().unwrap();
        assert_eq!(by[0].1.input, 20, "two responses, one per transcript");
    }
}
