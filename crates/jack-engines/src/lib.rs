//! Engine adapters: run an agent CLI headless and normalize what it did.
//!
//! Every engine takes the same [`RunSpec`] and returns the same
//! [`RunResult`]: session id (for resuming), a summary, the files it changed
//! (from git), token usage and cost when the engine reports it. Parsing is
//! a pure function per engine over its stream-json lines, so it is tested
//! against recorded output (design doc, Problem 5).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Plan / read-only: no edits, no shell.
    Read,
    /// File edits allowed, shell commands refused.
    Edit,
    /// Edits and shell commands.
    Full,
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub prompt: String,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub access: Access,
    /// Attach jack's MCP server and route file reads through it.
    pub jack_tools: Option<PathBuf>,
    pub timeout: Duration,
    /// Extra environment variables for the agent (e.g. its port range).
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub ok: bool,
}

/// What an engine run produced, normalized across engines.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunResult {
    pub engine: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// The agent's final message.
    pub summary: String,
    pub is_error: bool,
    pub usage: Usage,
    /// API-equivalent cost of the whole session so far, when the engine
    /// reports it (Claude Code does; it is cumulative across resumes).
    pub session_cost_usd: Option<f64>,
    pub num_turns: Option<i64>,
    pub duration_ms: i64,
    pub tool_calls: Vec<ToolCall>,
    pub files_changed: Vec<String>,
    pub diff_stat: String,
    /// Claude's 5-hour quota window utilization (0..1), when reported.
    pub quota_5h: Option<f64>,
}

/// Progress events, for live display while a run is going.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started { session_id: String, model: Option<String> },
    Text(String),
    Tool { name: String, detail: String },
    ToolDone { name: String, ok: bool },
}

/// Accumulates one run's stream into a [`RunResult`].
#[derive(Default)]
pub struct Acc {
    pub result: RunResult,
    last_text: Option<String>,
    got_result: bool,
    /// tool_use id -> index in result.tool_calls (Claude)
    pending: BTreeMap<String, usize>,
}

impl Acc {
    fn finish(mut self) -> RunResult {
        if self.result.summary.is_empty()
            && let Some(t) = self.last_text.take()
        {
            self.result.summary = t;
        }
        self.result
    }
}

fn short(s: &str, n: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n { one_line } else { format!("{}…", one_line.chars().take(n).collect::<String>()) }
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// A one-line description of a tool call's input, for progress display.
fn tool_detail(input: &Value) -> String {
    for k in ["file_path", "path", "command", "pattern", "globPattern", "query", "url", "description"] {
        if let Some(s) = input.get(k).and_then(Value::as_str) {
            return short(s, 80);
        }
    }
    String::new()
}

// ------------------------------------------------------------ Claude Code

/// Parse one stream-json line from `claude -p --output-format stream-json --verbose`.
pub fn parse_claude(d: &Value, acc: &mut Acc) -> Vec<Event> {
    let mut ev = Vec::new();
    match (d["type"].as_str(), d["subtype"].as_str()) {
        (Some("system"), Some("init")) => {
            let sid = d["session_id"].as_str().unwrap_or("").to_string();
            acc.result.session_id = Some(sid.clone());
            acc.result.model = d["model"].as_str().map(str::to_string);
            ev.push(Event::Started { session_id: sid, model: acc.result.model.clone() });
        }
        (Some("assistant"), _) if d["parent_tool_use_id"].is_null() => {
            for b in d["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => {
                        let t = b["text"].as_str().unwrap_or("").to_string();
                        if !t.trim().is_empty() {
                            ev.push(Event::Text(t.clone()));
                            acc.last_text = Some(t);
                        }
                    }
                    Some("tool_use") => {
                        let name = b["name"].as_str().unwrap_or("?").to_string();
                        if let Some(id) = b["id"].as_str() {
                            acc.pending.insert(id.to_string(), acc.result.tool_calls.len());
                        }
                        acc.result.tool_calls.push(ToolCall { name: name.clone(), ok: true });
                        ev.push(Event::Tool { name, detail: tool_detail(&b["input"]) });
                    }
                    _ => {}
                }
            }
        }
        (Some("user"), _) => {
            for b in d["message"]["content"].as_array().into_iter().flatten() {
                if b["type"].as_str() == Some("tool_result")
                    && let Some(idx) = b["tool_use_id"].as_str().and_then(|id| acc.pending.remove(id))
                {
                    let ok = !b["is_error"].as_bool().unwrap_or(false);
                    acc.result.tool_calls[idx].ok = ok;
                    ev.push(Event::ToolDone { name: acc.result.tool_calls[idx].name.clone(), ok });
                }
            }
        }
        (Some("rate_limit_event"), _) => {
            if let Some(u) = d.pointer("/rate_limit_info/unifiedWindows/five_hour/utilization").and_then(Value::as_f64) {
                acc.result.quota_5h = Some(u);
            }
        }
        (Some("result"), sub) => {
            acc.got_result = true;
            let r = &mut acc.result;
            r.is_error = d["is_error"].as_bool().unwrap_or(sub != Some("success"));
            if let Some(t) = d["result"].as_str() {
                r.summary = t.to_string();
            }
            if r.summary.is_empty() && r.is_error {
                r.summary = format!("claude ended with {}", sub.unwrap_or("an error"));
            }
            if let Some(s) = d["session_id"].as_str() {
                r.session_id = Some(s.to_string());
            }
            let u = &d["usage"];
            r.usage = Usage {
                input: i(u, "input_tokens"),
                output: i(u, "output_tokens"),
                cache_read: i(u, "cache_read_input_tokens"),
                cache_write: i(u, "cache_creation_input_tokens"),
            };
            r.session_cost_usd = d["total_cost_usd"].as_f64();
            r.num_turns = d["num_turns"].as_i64();
            r.duration_ms = i(d, "duration_ms");
        }
        _ => {}
    }
    ev
}

/// jack MCP tools a run may use without asking, by access level.
pub fn jack_tools_allowed(access: Access) -> Vec<String> {
    let mut tools = vec!["jack_read", "jack_search", "recall", "checkpoint", "jack_def", "jack_refs", "jack_diagnostics"];
    if access != Access::Read {
        tools.push("jack_rename");
    }
    if access == Access::Full {
        tools.push("jack_sh");
    }
    tools.into_iter().map(|t| format!("mcp__jack__{t}")).collect()
}

pub fn claude_command(spec: &RunSpec) -> Command {
    let mut c = Command::new("claude");
    c.args(["-p", "--output-format", "stream-json", "--verbose"]);
    c.args(["--permission-mode", match spec.access {
        Access::Read => "plan",
        Access::Edit => "acceptEdits",
        Access::Full => "bypassPermissions",
    }]);
    if let Some(m) = &spec.model {
        c.args(["--model", m]);
    }
    if let Some(id) = &spec.resume {
        c.args(["--resume", id]);
    }
    match &spec.jack_tools {
        Some(jack) => {
            let cfg = serde_json::json!({"mcpServers": {"jack": {
                "command": jack, "args": ["mcp", "--root", spec.cwd]
            }}});
            c.args(["--mcp-config", &cfg.to_string(), "--strict-mcp-config", "--disallowedTools", "Read"]);
            // Headless runs can't approve tools, so pre-allow the jack tools
            // this access level permits: jack_sh is a shell, jack_rename edits.
            c.arg("--allowedTools").args(jack_tools_allowed(spec.access));
        }
        None => {
            c.arg("--strict-mcp-config");
        }
    }
    c.arg("--").arg(&spec.prompt);
    c
}

// ------------------------------------------------------------ Cursor Agent

/// `readToolCall` -> `read`
fn cursor_tool_name(tc: &Value) -> (String, Value) {
    tc.as_object()
        .and_then(|o| o.iter().find(|(k, _)| k.ends_with("ToolCall")))
        .map(|(k, v)| (k.trim_end_matches("ToolCall").to_string(), v.clone()))
        .unwrap_or_else(|| ("?".to_string(), Value::Null))
}

/// Parse one stream-json line from `cursor-agent -p --output-format stream-json`.
pub fn parse_cursor(d: &Value, acc: &mut Acc) -> Vec<Event> {
    let mut ev = Vec::new();
    match (d["type"].as_str(), d["subtype"].as_str()) {
        (Some("system"), Some("init")) => {
            let sid = d["session_id"].as_str().unwrap_or("").to_string();
            acc.result.session_id = Some(sid.clone());
            acc.result.model = d["model"].as_str().map(str::to_string);
            ev.push(Event::Started { session_id: sid, model: acc.result.model.clone() });
        }
        (Some("assistant"), _) => {
            for b in d["message"]["content"].as_array().into_iter().flatten() {
                if b["type"].as_str() == Some("text") {
                    let t = b["text"].as_str().unwrap_or("").to_string();
                    if !t.trim().is_empty() {
                        ev.push(Event::Text(t.clone()));
                        acc.last_text = Some(t);
                    }
                }
            }
        }
        (Some("tool_call"), Some("started")) => {
            let (name, body) = cursor_tool_name(&d["tool_call"]);
            ev.push(Event::Tool { name, detail: tool_detail(&body["args"]) });
        }
        (Some("tool_call"), Some("completed")) => {
            let (name, body) = cursor_tool_name(&d["tool_call"]);
            let ok = body["result"].get("success").is_some();
            acc.result.tool_calls.push(ToolCall { name: name.clone(), ok });
            ev.push(Event::ToolDone { name, ok });
        }
        (Some("result"), sub) => {
            acc.got_result = true;
            let r = &mut acc.result;
            r.is_error = d["is_error"].as_bool().unwrap_or(sub != Some("success"));
            // Cursor's `result` joins every assistant message with no
            // separator, so the summary is the last message instead.
            if r.is_error {
                r.summary = d["result"].as_str().unwrap_or("cursor-agent reported an error").to_string();
            }
            if let Some(s) = d["session_id"].as_str() {
                r.session_id = Some(s.to_string());
            }
            let u = &d["usage"];
            r.usage = Usage {
                input: i(u, "inputTokens"),
                output: i(u, "outputTokens"),
                cache_read: i(u, "cacheReadTokens"),
                cache_write: i(u, "cacheWriteTokens"),
            };
            r.duration_ms = i(d, "duration_ms");
        }
        _ => {}
    }
    ev
}

pub fn cursor_command(spec: &RunSpec) -> Command {
    let mut c = Command::new("cursor-agent");
    c.args(["-p", "--output-format", "stream-json", "--trust"]);
    c.arg("--workspace").arg(&spec.cwd);
    match spec.access {
        Access::Read => {
            c.args(["--mode", "plan"]);
        }
        Access::Edit => {}
        Access::Full => {
            c.arg("--force");
        }
    }
    if spec.jack_tools.is_some() {
        // The orchestrator writes .cursor/mcp.json into the worktree.
        c.arg("--approve-mcps");
    }
    if let Some(m) = &spec.model {
        c.args(["--model", m]);
    }
    if let Some(id) = &spec.resume {
        c.args(["--resume", id]);
    }
    c.arg(&spec.prompt);
    c
}

// ------------------------------------------------------------ engines

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Claude,
    Cursor,
}

impl Engine {
    pub fn parse(name: &str) -> Option<Engine> {
        match name {
            "claude" | "claude-code" => Some(Engine::Claude),
            "cursor" | "cursor-agent" => Some(Engine::Cursor),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Claude => "claude",
            Engine::Cursor => "cursor",
        }
    }

    pub fn command(self, spec: &RunSpec) -> Command {
        match self {
            Engine::Claude => claude_command(spec),
            Engine::Cursor => cursor_command(spec),
        }
    }

    pub fn parse_line(self, d: &Value, acc: &mut Acc) -> Vec<Event> {
        match self {
            Engine::Claude => parse_claude(d, acc),
            Engine::Cursor => parse_cursor(d, acc),
        }
    }
}

/// Parse a whole recorded stream (used by tests and for re-reading logs).
pub fn parse_stream(engine: Engine, text: &str) -> RunResult {
    let mut acc = Acc::default();
    acc.result.engine = engine.name().to_string();
    for line in text.lines() {
        if let Ok(d) = serde_json::from_str::<Value>(line) {
            engine.parse_line(&d, &mut acc);
        }
    }
    acc.finish()
}

// ------------------------------------------------------------ git

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(cwd).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Dirty files and their content hashes, so changes made by the run can be
/// told apart from changes that were already there.
fn snapshot(cwd: &Path) -> Option<BTreeMap<String, String>> {
    let status = git(cwd, &["status", "--porcelain", "-z", "--untracked-files=all"])?;
    let mut map = BTreeMap::new();
    for entry in status.split('\0').filter(|e| e.len() > 3) {
        let path = entry[3..].to_string();
        let hash = git(cwd, &["hash-object", "--", &path]).map(|h| h.trim().to_string()).unwrap_or_default();
        map.insert(path, hash);
    }
    Some(map)
}

// ------------------------------------------------------------ running

/// Run an engine to completion. `on_event` sees progress as it happens;
/// every raw stream line is appended to `log`.
pub fn run(engine: Engine, spec: &RunSpec, log: &Path, mut on_event: impl FnMut(&Event)) -> Result<RunResult> {
    let before = snapshot(&spec.cwd);
    let started = Instant::now();
    let mut child = engine
        .command(spec)
        .current_dir(&spec.cwd)
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", engine.name()))?;
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    // Enforce the timeout from a watchdog thread; the reader below blocks.
    let pid = child.id();
    let timeout = spec.timeout;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done2 = done.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if done2.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });

    let mut logf = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut acc = Acc::default();
    acc.result.engine = engine.name().to_string();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let _ = writeln!(logf, "{line}");
        if let Ok(d) = serde_json::from_str::<Value>(&line) {
            for e in engine.parse_line(&d, &mut acc) {
                on_event(&e);
            }
        }
    }
    let status = child.wait()?;
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let stderr = err_thread.join().unwrap_or_default();
    let timed_out = started.elapsed() >= spec.timeout;
    let got_result = acc.got_result;
    let mut r = acc.finish();
    if r.duration_ms == 0 {
        r.duration_ms = started.elapsed().as_millis() as i64;
    }
    if !got_result {
        r.is_error = true;
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        r.summary = if timed_out {
            format!("{} timed out after {}s", engine.name(), spec.timeout.as_secs())
        } else {
            format!("{} exited with {status} and no result. {}", engine.name(), tail.trim())
        };
    }

    if let (Some(before), Some(after)) = (before, snapshot(&spec.cwd)) {
        r.files_changed = after
            .iter()
            .filter(|(p, h)| before.get(*p) != Some(*h))
            .map(|(p, _)| p.clone())
            .chain(before.keys().filter(|p| !after.contains_key(*p)).cloned())
            .collect();
        if !r.files_changed.is_empty() {
            let mut args = vec!["diff", "--stat", "--"];
            args.extend(r.files_changed.iter().map(String::as_str));
            r.diff_stat = git(&spec.cwd, &args).unwrap_or_default().trim_end().to_string();
        }
    }
    Ok(r)
}

/// Fail early with a clear message if an engine's CLI is missing.
pub fn check_installed(engine: Engine) -> Result<()> {
    let cmd = match engine {
        Engine::Claude => "claude",
        Engine::Cursor => "cursor-agent",
    };
    let found = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()));
    if !found {
        bail!("{cmd} not found on PATH");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_fixture() {
        let r = parse_stream(Engine::Claude, include_str!("../tests/fixtures/claude.jsonl"));
        assert_eq!(r.session_id.as_deref(), Some("4f811ad5-69dc-4881-82ff-74de96b321b3"));
        assert_eq!(r.model.as_deref(), Some("claude-opus-5-5"));
        assert!(!r.is_error);
        assert!(r.summary.contains("a + b"), "{}", r.summary);
        assert_eq!(r.usage, Usage { input: 6, output: 345, cache_read: 47362, cache_write: 10426 });
        assert!((r.session_cost_usd.unwrap() - 0.0998044).abs() < 1e-6);
        assert_eq!(r.num_turns, Some(3));
        assert_eq!(r.quota_5h, Some(0.11), "last rate_limit_event wins");
        let tools: Vec<_> = r.tool_calls.iter().map(|t| (t.name.as_str(), t.ok)).collect();
        assert_eq!(tools, vec![("Read", true), ("Edit", true)]);
    }

    #[test]
    fn cursor_fixture() {
        let r = parse_stream(Engine::Cursor, include_str!("../tests/fixtures/cursor.jsonl"));
        assert_eq!(r.session_id.as_deref(), Some("fbb19f8a-2780-49dc-8789-eb8be3f3a049"));
        assert_eq!(r.model.as_deref(), Some("Grok 4.7 256K High Fast"));
        assert!(!r.is_error);
        // The last message, not Cursor's run-together `result` string.
        assert_eq!(r.summary, "`add()` now returns the sum of its two arguments.");
        assert_eq!(r.usage, Usage { input: 9075, output: 308, cache_read: 49664, cache_write: 0 });
        assert_eq!(r.session_cost_usd, None);
        let tools: Vec<_> = r.tool_calls.iter().map(|t| (t.name.as_str(), t.ok)).collect();
        assert_eq!(tools, vec![("glob", true), ("read", true), ("edit", true)]);
    }

    #[test]
    fn commands_map_access_levels() {
        let spec = |access| RunSpec {
            prompt: "-starts with a dash".into(),
            cwd: "/w".into(),
            resume: Some("abc".into()),
            model: None,
            access,
            jack_tools: None,
            timeout: Duration::from_secs(1),
            env: vec![],
        };
        let args = |c: Command| c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let c = args(claude_command(&spec(Access::Read)));
        assert!(c.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(c.windows(2).any(|w| w == ["--resume", "abc"]));
        assert_eq!(c[c.len() - 2..], ["--", "-starts with a dash"]);
        let mut with_jack = spec(Access::Edit);
        with_jack.jack_tools = Some("/bin/jack".into());
        let j = args(claude_command(&with_jack));
        assert!(j.contains(&"mcp__jack__jack_rename".to_string()), "edit may rename");
        assert!(!j.contains(&"mcp__jack__jack_sh".to_string()), "edit must not get a shell");
        assert!(!jack_tools_allowed(Access::Read).iter().any(|t| t.ends_with("rename") || t.ends_with("_sh")));
        let k = args(cursor_command(&spec(Access::Full)));
        assert!(k.contains(&"--force".to_string()) && k.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(!args(cursor_command(&spec(Access::Edit))).contains(&"--force".to_string()));
    }

    #[test]
    fn snapshot_tells_new_changes_from_old() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let g = |a: &[&str]| assert!(Command::new("git").args(a).current_dir(d).output().unwrap().status.success());
        g(&["init", "-q"]);
        std::fs::write(d.join("a.txt"), "1").unwrap();
        std::fs::write(d.join("b.txt"), "1").unwrap();
        g(&["add", "."]);
        g(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "x"]);
        std::fs::write(d.join("a.txt"), "already dirty").unwrap();
        let before = snapshot(d).unwrap();
        std::fs::write(d.join("b.txt"), "2").unwrap();
        std::fs::write(d.join("new.txt"), "n").unwrap();
        let after = snapshot(d).unwrap();
        let changed: Vec<_> = after.iter().filter(|(p, h)| before.get(*p) != Some(*h)).map(|(p, _)| p.as_str()).collect();
        assert_eq!(changed, vec!["b.txt", "new.txt"]);
    }
}
