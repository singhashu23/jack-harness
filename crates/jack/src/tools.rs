//! jack's tools: session-aware reading, searching and shell output shaping.
//! Every result is shaped to a token budget; the full output is kept in the
//! session store under a handle the model can `recall` (design doc, Problem 4).

use anyhow::{anyhow, bail, Context, Result};
use jack_read::{
    estimate_tokens, find_symbols, number_lines, outline, render_outline, trim_output,
    unified_diff, Lang,
};
use jack_store::{handle_for, Store, View};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default per-result budget, in estimated tokens.
pub const DEFAULT_BUDGET: usize = 2000;
/// Hard cap even when the model asks for a full file.
const HARD_CAP_TOKENS: usize = 25_000;
const SH_MAX_LINES: usize = 80;
const MAX_LINE_CHARS: usize = 400;

pub struct Output {
    pub text: String,
    pub is_error: bool,
}

pub struct Tools {
    store: Store,
    root: PathBuf,
    budget: usize,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Parse "a-b", "a-", or "a" into a 1-based inclusive range.
fn parse_range(s: &str, total: usize) -> Result<(usize, usize)> {
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (s.trim(), s.trim()),
    };
    let start: usize = a.parse().with_context(|| format!("bad line range {s:?}"))?;
    let end: usize = if b.is_empty() { total } else { b.parse().with_context(|| format!("bad line range {s:?}"))? };
    if start == 0 || start > end {
        bail!("bad line range {s:?} (lines are 1-based, start <= end)");
    }
    Ok((start, end.min(total.max(1))))
}

fn cap_line_len(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.chars().count() > MAX_LINE_CHARS {
                let cut: String = l.chars().take(MAX_LINE_CHARS).collect();
                format!("{cut} [… line cut]")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tools {
    pub fn new(store: Store, root: PathBuf) -> Tools {
        Tools { store, root, budget: DEFAULT_BUDGET }
    }

    #[cfg(test)]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Run a tool by name, log it, and return its shaped output.
    pub fn call(&self, name: &str, args: &Value) -> Output {
        let result = match name {
            "jack_read" => self.jack_read(args),
            "jack_sh" => self.jack_sh(args),
            "jack_search" => self.jack_search(args),
            "recall" => self.recall(args),
            "checkpoint" => self.checkpoint(args),
            _ => Err(anyhow!("unknown tool {name}")),
        };
        let (text, raw, is_error) = match result {
            Ok((text, raw)) => (text, raw, false),
            Err(e) => (format!("error: {e:#}"), 0, true),
        };
        let mut logged = args.clone();
        if let Some(note) = logged.get_mut("note") {
            *note = json!("<see checkpoint event>");
        }
        let _ = self.store.log_event("tool_call", Some(name), &logged, raw.max(text.len()), text.len());
        Output { text, is_error }
    }

    fn resolve(&self, p: &str) -> Result<(PathBuf, String)> {
        let path = if Path::new(p).is_absolute() { PathBuf::from(p) } else { self.root.join(p) };
        let path = path.canonicalize().with_context(|| format!("{p}: not found"))?;
        let shown = path
            .strip_prefix(&self.root)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        Ok((path, shown))
    }

    /// Record a non-full view without downgrading a richer view of the same
    /// contents: the model still holds that full file or outline.
    fn record_partial(&self, key: &str, hash: &str, handle: &str, view: View) -> Result<()> {
        if let Some(last) = self.store.last_read(key)?
            && last.hash == hash
            && (last.view == View::Full || (last.view == View::Outline && view == View::Part))
        {
            return Ok(());
        }
        self.store.record_read(key, hash, handle, view)?;
        Ok(())
    }

    fn jack_read(&self, args: &Value) -> Result<(String, usize)> {
        let p = arg_str(args, "path").ok_or_else(|| anyhow!("missing `path`"))?;
        let (path, shown) = self.resolve(p)?;
        if path.is_dir() {
            bail!("{shown} is a directory; use jack_sh with `ls` or jack_search");
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {shown}"))?;
        let src = String::from_utf8(bytes).map_err(|_| anyhow!("{shown} is not UTF-8 text"))?;
        let key = path.display().to_string();
        let hash = handle_for(src.as_bytes());
        let total = src.lines().count();
        // What a plain numbered read of the whole file would have cost.
        let raw = src.len() + total * 7;
        let lang = Lang::from_path(&path);
        let handle = self.store.put_blob(&src)?;
        let budget_bytes = self.budget * 4;

        // Explicit line range.
        if let Some(range) = arg_str(args, "lines") {
            let (a, b) = parse_range(range, total)?;
            let mut body = number_lines(&src, a, b);
            let mut note = String::new();
            if body.len() > budget_bytes * 4 {
                let keep = (b - a + 1) * (budget_bytes * 4) / body.len();
                let b2 = a + keep.max(1) - 1;
                body = number_lines(&src, a, b2);
                note = format!("[cut at line {b2}; ask for lines={}-{b} to continue]\n", b2 + 1);
            }
            self.record_partial(&key, &hash, &handle, View::Part)?;
            return Ok((format!("{shown} L{a}-{b} of {total}:\n{body}{note}"), raw));
        }

        // One symbol (or several with the same name).
        if let Some(sym) = arg_str(args, "symbol") {
            let lang = lang.ok_or_else(|| anyhow!("no parser for {shown}; use lines=a-b"))?;
            let syms = outline(&src, lang);
            let hits = find_symbols(&syms, sym);
            if hits.is_empty() {
                let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).take(40).collect();
                bail!("no symbol {sym:?} in {shown}. Symbols: {}", names.join(", "));
            }
            let mut out = String::new();
            for h in hits {
                out.push_str(&format!("{shown} L{}-{} ({} {}):\n", h.start_line, h.end_line, h.kind, h.name));
                out.push_str(&number_lines(&src, h.start_line, h.end_line));
            }
            if estimate_tokens(&out) > HARD_CAP_TOKENS {
                let (t, _) = trim_output(&out, 400);
                out = t + "[symbol body cut; use lines=a-b for the rest]\n";
            }
            self.record_partial(&key, &hash, &handle, View::Part)?;
            return Ok((out, raw));
        }

        let full = arg_bool(args, "full");
        let last = self.store.last_read(&key)?;

        if !full
            && let Some(r) = &last {
                if r.hash == hash && r.view == View::Full {
                    return Ok((format!(
                        "{shown}: unchanged since read #{} (hash {}). You already have the full file. \
                         Use symbol= or lines= to re-view part of it, or full=true if it is no longer in your context.",
                        r.seq, &hash[..8]
                    ), raw));
                }
                if r.hash == hash && r.view == View::Outline {
                    return Ok((format!(
                        "{shown}: unchanged since read #{}; its outline was shown then. \
                         Use symbol= or lines=a-b, or full=true.",
                        r.seq
                    ), raw));
                }
                if r.hash != hash && r.view == View::Full
                    && let Some(old) = self.store.get_blob(&r.handle)? {
                        let diff = unified_diff(&old, &src, &shown);
                        if estimate_tokens(&diff) <= self.budget {
                            let seq = self.store.record_read(&key, &hash, &handle, View::Full)?;
                            return Ok((format!(
                                "{shown}: changed since read #{} (now read #{seq}, {total} lines). Diff:\n{diff}",
                                r.seq
                            ), raw));
                        }
                    }
            }

        let tokens = estimate_tokens(&src);
        if full || tokens <= self.budget || lang.is_none() {
            let cap = if full || lang.is_some() { HARD_CAP_TOKENS } else { self.budget };
            if tokens <= cap {
                let seq = self.store.record_read(&key, &hash, &handle, View::Full)?;
                return Ok((format!("{shown} ({total} lines, read #{seq}):\n{}", number_lines(&src, 1, total)), raw));
            }
            // Too big to show whole: first chunk, then ask for ranges.
            let lines_fit = (total * cap / tokens).max(1);
            self.record_partial(&key, &hash, &handle, View::Part)?;
            return Ok((format!(
                "{shown} ({total} lines, ~{tokens} tokens; too large to show whole). L1-{lines_fit}:\n{}\
                 [continue with lines={}-]\n",
                number_lines(&src, 1, lines_fit),
                lines_fit + 1
            ), raw));
        }

        let lang = lang.expect("checked above");
        let syms = outline(&src, lang);
        let seq = self.store.record_read(&key, &hash, &handle, View::Outline)?;
        let mut rendered = render_outline(&shown, lang, total, &syms);
        if estimate_tokens(&rendered) > self.budget * 2 {
            rendered = trim_output(&rendered, 300).0;
        }
        Ok((format!(
            "{rendered}[read #{seq}: file is ~{tokens} tokens, so only its outline is shown. \
             Read parts with symbol=NAME or lines=a-b, or the whole file with full=true.]\n"
        ), raw))
    }

    fn jack_sh(&self, args: &Value) -> Result<(String, usize)> {
        let cmd = arg_str(args, "cmd").ok_or_else(|| anyhow!("missing `cmd`"))?;
        let timeout = args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(120).clamp(1, 600);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {cmd}\n}} 2>&1"))
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning sh")?;
        let mut stdout = child.stdout.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if start.elapsed() > Duration::from_secs(timeout) {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
        let raw = out.len();
        let head = match status {
            Some(s) => match s.code() {
                Some(c) => format!("exit {c}"),
                None => "killed by signal".to_string(),
            },
            None => format!("timed out after {timeout}s (killed)"),
        };
        if out.trim().is_empty() {
            return Ok((format!("{head}, no output"), raw));
        }
        let handle = self.store.put_blob(&out)?;
        let (shown, cut) = trim_output(&cap_line_len(&out), SH_MAX_LINES);
        let mut text = format!("{head}, {} lines:\n{shown}", out.lines().count());
        if cut {
            text.push_str(&format!("[full output stored: recall(handle=\"{handle}\", lines=\"a-b\")]\n"));
        }
        Ok((text, raw))
    }

    fn jack_search(&self, args: &Value) -> Result<(String, usize)> {
        let query = arg_str(args, "query").ok_or_else(|| anyhow!("missing `query`"))?;
        let path = arg_str(args, "path").unwrap_or(".");
        let max_files = args.get("max_files").and_then(Value::as_u64).unwrap_or(10) as usize;
        let mut cmd = Command::new("rg");
        cmd.args(["-n", "--no-heading", "--color", "never", "-m", "5", "--max-columns", "200", "--max-columns-preview"]);
        if arg_bool(args, "fixed") {
            cmd.arg("-F");
        }
        let out = cmd.arg("--").arg(query).arg(path).current_dir(&self.root).output().context("running rg")?;
        if !out.status.success() && out.stdout.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.trim().is_empty() {
                return Ok((format!("no matches for {query:?}"), 0));
            }
            bail!("rg: {}", err.trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let raw = text.len();
        let mut files: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, ':');
            let (Some(f), Some(n), Some(t)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let entry = format!("  {n}: {}", t.trim());
            match files.last_mut() {
                Some((last, v)) if last == f => v.push(entry),
                _ => files.push((f.to_string(), vec![entry])),
            }
        }
        let total: usize = files.iter().map(|(_, v)| v.len()).sum();
        let mut s = format!(
            "{} files with matches ({total} lines, max 5 per file){}:\n",
            files.len(),
            if files.len() > max_files { format!(", showing first {max_files}") } else { String::new() }
        );
        for (f, v) in files.iter().take(max_files) {
            s.push_str(f);
            s.push('\n');
            for e in v {
                s.push_str(e);
                s.push('\n');
            }
        }
        if files.len() > max_files {
            let rest: Vec<&str> = files[max_files..].iter().map(|(f, _)| f.as_str()).take(30).collect();
            s.push_str(&format!("also in: {}\n", rest.join(", ")));
        }
        Ok((s, raw))
    }

    fn recall(&self, args: &Value) -> Result<(String, usize)> {
        if let Some(h) = arg_str(args, "handle") {
            let blob = self.store.get_blob(h)?.ok_or_else(|| anyhow!("no stored output with handle {h}"))?;
            let total = blob.lines().count();
            let (a, b) = match arg_str(args, "lines") {
                Some(r) => parse_range(r, total)?,
                None => (1, total.min(200)),
            };
            let b = b.min(a + 299);
            let mut s = format!("handle {h} L{a}-{b} of {total}:\n{}", number_lines(&blob, a, b));
            if b < total {
                s.push_str(&format!("[more: lines={}-]\n", b + 1));
            }
            let raw = blob.len();
            return Ok((s, raw));
        }
        if let Some(q) = arg_str(args, "query") {
            let hits = self.store.search(q, 8)?;
            if hits.is_empty() {
                return Ok((format!("nothing stored this session matches {q:?}"), 0));
            }
            let mut s = format!("{} matches in this session's stored outputs:\n", hits.len());
            for h in hits {
                s.push_str(&format!("- {}: {}\n", h.handle, h.snippet.replace('\n', " ")));
            }
            s.push_str("[open one with recall(handle=..., lines=a-b)]\n");
            return Ok((s, 0));
        }
        bail!("give `handle` (optionally with `lines`) or `query`")
    }

    fn checkpoint(&self, args: &Value) -> Result<(String, usize)> {
        let note = arg_str(args, "note").ok_or_else(|| anyhow!("missing `note`"))?;
        self.store.log_event("checkpoint", None, &json!({ "note": note }), 0, 0)?;
        self.store.put_blob(&format!("checkpoint: {note}"))?;
        Ok(("saved".to_string(), 0))
    }
}

/// MCP tool definitions. Descriptions steer the model toward the cheap paths.
pub fn tool_list() -> Value {
    json!([
        {
            "name": "jack_read",
            "description": "Read a file, token-efficiently. Small files come back whole with line numbers. Large files return an outline (symbols with line ranges); then read only what you need with symbol= or lines=. Re-reading a file you already saw returns 'unchanged' or just a diff. Prefer this over any other file-reading tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the project root or absolute"},
                    "symbol": {"type": "string", "description": "Return only this symbol, e.g. 'verify_token', 'Store::open', 'MyClass.method'"},
                    "lines": {"type": "string", "description": "1-based inclusive line range, e.g. '120-180' or '300-'"},
                    "full": {"type": "boolean", "description": "Return the whole file even if large or unchanged"}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "name": "jack_search",
            "description": "Search the codebase with ripgrep. Results are grouped by file, at most 5 lines per file. Use it to find where something is defined or used before reading files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Regex (or a literal string with fixed=true)"},
                    "path": {"type": "string", "description": "Directory or file to search, default the project root"},
                    "fixed": {"type": "boolean", "description": "Treat query as a literal string"},
                    "max_files": {"type": "integer", "description": "Files to show, default 10"}
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "jack_sh",
            "description": "Run a shell command in the project root. Long output is trimmed to its head, its error and warning lines, and its tail; the full output is stored and can be paged with recall. Prefer this for builds, tests and other commands with long output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Shell command (run with sh -c)"},
                    "timeout_secs": {"type": "integer", "description": "Kill after this many seconds, default 120, max 600"}
                },
                "required": ["cmd"],
                "additionalProperties": false
            }
        },
        {
            "name": "recall",
            "description": "Get back output that jack trimmed or stored this session: page through a stored output by handle, or search everything stored by keyword.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "handle": {"type": "string", "description": "Handle printed by an earlier tool result"},
                    "lines": {"type": "string", "description": "Line range within that output, e.g. '200-260'"},
                    "query": {"type": "string", "description": "Keywords to search all stored outputs"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "checkpoint",
            "description": "Save a short note about progress: a decision made, a fact learned, or what is left to do. jack uses these notes to brief later sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {"note": {"type": "string"}},
                "required": ["note"],
                "additionalProperties": false
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Tools) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let tools = Tools::new(Store::open_in_memory().unwrap(), root);
        (dir, tools)
    }

    fn text(tools: &Tools, name: &str, args: Value) -> String {
        let o = tools.call(name, &args);
        assert!(!o.is_error, "{}", o.text);
        o.text
    }

    #[test]
    fn small_file_then_unchanged_then_diff() {
        let (dir, t) = setup();
        let f = dir.path().join("a.py");
        std::fs::write(&f, "def f():\n    return 1\n").unwrap();
        let first = text(&t, "jack_read", json!({"path": "a.py"}));
        assert!(first.contains("1\tdef f():"), "{first}");
        let second = text(&t, "jack_read", json!({"path": "a.py"}));
        assert!(second.contains("unchanged since read #1"), "{second}");
        std::fs::write(&f, "def f():\n    return 2\n").unwrap();
        let third = text(&t, "jack_read", json!({"path": "a.py"}));
        assert!(third.contains("changed since read #1") && third.contains("+    return 2"), "{third}");
    }

    #[test]
    fn large_file_gets_outline_then_symbol() {
        let (dir, t) = setup();
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("fn func_{i}() {{\n    let x = {i}; // padding padding padding\n}}\n"));
        }
        std::fs::write(dir.path().join("big.rs"), &src).unwrap();
        let o = text(&t, "jack_read", json!({"path": "big.rs"}));
        assert!(o.contains("outline") && o.contains("fn func_7  L22-24"), "{}", &o[..300]);
        assert!(o.len() < src.len() / 2);
        let s = text(&t, "jack_read", json!({"path": "big.rs", "symbol": "func_7"}));
        assert!(s.contains("22\tfn func_7()"), "{s}");
        // A symbol read must not make jack forget the outline was shown.
        let again = text(&t, "jack_read", json!({"path": "big.rs"}));
        assert!(again.contains("unchanged since read #1"), "{again}");
    }

    #[test]
    fn sh_trims_and_recall_pages() {
        let (_dir, t) = setup();
        let o = text(&t, "jack_sh", json!({"cmd": "seq 1 500; echo 'error: boom' >&2"}));
        assert!(o.starts_with("exit 0, 501 lines"), "{o}");
        assert!(o.contains("error: boom"));
        let handle = o.split("handle=\"").nth(1).unwrap().split('"').next().unwrap();
        let r = text(&t, "recall", json!({"handle": handle, "lines": "250-252"}));
        assert!(r.contains("250\t250") && r.contains("252\t252"), "{r}");
        let q = text(&t, "recall", json!({"query": "boom"}));
        assert!(q.contains(handle), "{q}");
    }

    #[test]
    fn stats_show_savings() {
        let (dir, t) = setup();
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("def f{i}():\n    return {i}  # padding padding padding\n"));
        }
        std::fs::write(dir.path().join("m.py"), &src).unwrap();
        text(&t, "jack_read", json!({"path": "m.py"}));
        let st = t.store().stats().unwrap();
        let (_, s) = st.iter().find(|(n, _)| n == "jack_read").unwrap();
        assert!(s.sent_bytes * 2 < s.raw_bytes, "{s:?}");
    }

    #[test]
    fn errors_are_reported_not_panics() {
        let (_dir, t) = setup();
        assert!(t.call("jack_read", &json!({"path": "missing.rs"})).is_error);
        assert!(t.call("jack_read", &json!({})).is_error);
        assert!(t.call("nope", &json!({})).is_error);
    }
}
