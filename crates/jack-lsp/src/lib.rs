//! A small Language Server Protocol client for jack.
//!
//! Starts language servers on demand (one per language per jack session),
//! keeps them running, and answers the semantic questions tree-sitter can't:
//! definition, references, rename, diagnostics. Written against the LSP spec
//! with raw JSON, so it has no dependency on any LSP library.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- framing

/// Write one LSP message: `Content-Length` header, blank line, JSON body.
pub fn write_msg(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let body = v.to_string();
    write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    w.flush()
}

/// Read one LSP message. `Ok(None)` at end of stream.
pub fn read_msg(r: &mut impl BufRead) -> Result<Option<Value>> {
    let mut len: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let t = line.trim_end();
        if t.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some((k, v)) = t.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = Some(v.trim().parse().context("bad Content-Length")?);
        }
    }
    let mut buf = vec![0; len.expect("loop exits only with a length")];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

// ---------------------------------------------------------------- URIs and columns

pub fn path_to_uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

pub fn uri_to_path(u: &str) -> Option<PathBuf> {
    let rest = u.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%' && i + 2 < rest.len() {
            let hex = std::str::from_utf8(&rest[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(rest[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// Byte column within `line` to an LSP `character` (UTF-8 bytes or UTF-16 units).
pub fn byte_to_lsp_col(line: &str, byte: usize, utf8: bool) -> usize {
    let mut b = byte.min(line.len());
    while !line.is_char_boundary(b) {
        b -= 1;
    }
    if utf8 { b } else { line[..b].encode_utf16().count() }
}

/// LSP `character` within `line` back to a byte column.
pub fn lsp_to_byte_col(line: &str, ch: usize, utf8: bool) -> usize {
    if utf8 {
        let mut b = ch.min(line.len());
        while !line.is_char_boundary(b) {
            b -= 1;
        }
        return b;
    }
    let mut units = 0;
    for (i, c) in line.char_indices() {
        if units >= ch {
            return i;
        }
        units += c.len_utf16();
    }
    line.len()
}

fn line_of(text: &str, row: usize) -> &str {
    text.split('\n').nth(row).unwrap_or("").trim_end_matches('\r')
}

// ---------------------------------------------------------------- edits

/// Byte offset of an LSP position in `text`.
fn offset(text: &str, pos: &Value, utf8: bool) -> Result<usize> {
    let row = pos["line"].as_u64().ok_or_else(|| anyhow!("bad position {pos}"))? as usize;
    let ch = pos["character"].as_u64().ok_or_else(|| anyhow!("bad position {pos}"))? as usize;
    let mut start = 0;
    for (i, l) in text.split('\n').enumerate() {
        if i == row {
            return Ok(start + lsp_to_byte_col(l.trim_end_matches('\r'), ch, utf8));
        }
        start += l.len() + 1;
    }
    // A position one past the last line means end of file.
    Ok(text.len())
}

/// Apply LSP `TextEdit`s to `text`. Edits must not overlap.
pub fn apply_text_edits(text: &str, edits: &[Value], utf8: bool) -> Result<String> {
    let mut spans = Vec::with_capacity(edits.len());
    for e in edits {
        let s = offset(text, &e["range"]["start"], utf8)?;
        let t = offset(text, &e["range"]["end"], utf8)?;
        let new = e["newText"].as_str().ok_or_else(|| anyhow!("edit without newText"))?;
        if s > t {
            bail!("edit range ends before it starts");
        }
        spans.push((s, t, new));
    }
    spans.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = text.to_string();
    let mut limit = usize::MAX;
    for (s, t, new) in spans {
        if t > limit {
            bail!("overlapping edits");
        }
        out.replace_range(s..t, new);
        limit = s;
    }
    Ok(out)
}

/// Files a `WorkspaceEdit` touches and how many edits each, without applying it.
pub fn workspace_edit_files(edit: &Value) -> Vec<(PathBuf, usize)> {
    let mut out = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(p) = uri_to_path(uri) {
                out.push((p, edits.as_array().map_or(0, Vec::len)));
            }
        }
    }
    if let Some(dcs) = edit.get("documentChanges").and_then(Value::as_array) {
        for dc in dcs {
            if let Some(p) = dc["textDocument"]["uri"].as_str().and_then(uri_to_path) {
                out.push((p, dc["edits"].as_array().map_or(0, Vec::len)));
            }
        }
    }
    out
}

/// Apply a `WorkspaceEdit` to disk. All new contents are computed before any
/// file is written, so a bad edit leaves every file untouched.
/// Returns (file, number of edits) per changed file.
pub fn apply_workspace_edit(edit: &Value, utf8: bool) -> Result<Vec<(PathBuf, usize)>> {
    let mut per_file: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            per_file.push((uri.clone(), edits.as_array().cloned().unwrap_or_default()));
        }
    }
    if let Some(dcs) = edit.get("documentChanges").and_then(Value::as_array) {
        for dc in dcs {
            if dc.get("kind").is_some() {
                bail!("this edit creates, renames or deletes files, which jack does not apply");
            }
            let uri = dc["textDocument"]["uri"].as_str().ok_or_else(|| anyhow!("edit without uri"))?;
            per_file.push((uri.to_string(), dc["edits"].as_array().cloned().unwrap_or_default()));
        }
    }
    let mut planned = Vec::new();
    for (uri, edits) in &per_file {
        if edits.is_empty() {
            continue;
        }
        let path = uri_to_path(uri).ok_or_else(|| anyhow!("not a file uri: {uri}"))?;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let new = apply_text_edits(&text, edits, utf8).with_context(|| format!("editing {}", path.display()))?;
        planned.push((path, new, edits.len()));
    }
    let mut done = Vec::new();
    for (path, new, n) in planned {
        std::fs::write(&path, new).with_context(|| format!("writing {}", path.display()))?;
        done.push((path, n));
    }
    Ok(done)
}

// ---------------------------------------------------------------- locations

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub path: PathBuf,
    /// 0-based.
    pub row: usize,
    /// LSP character (encoding per server).
    pub character: usize,
}

/// Parse `Location | Location[] | LocationLink[] | null`.
pub fn parse_locations(v: &Value) -> Vec<Loc> {
    let one = |x: &Value| -> Option<Loc> {
        let (uri, range) = match x.get("targetUri") {
            Some(u) => (u, x.get("targetSelectionRange").or_else(|| x.get("targetRange"))?),
            None => (x.get("uri")?, x.get("range")?),
        };
        Some(Loc {
            path: uri_to_path(uri.as_str()?)?,
            row: range["start"]["line"].as_u64()? as usize,
            character: range["start"]["character"].as_u64()? as usize,
        })
    };
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------- servers

#[derive(Debug, Clone, Copy)]
pub struct ServerCfg {
    /// One server per key per session.
    pub key: &'static str,
    /// Candidate executables, first one found on PATH wins.
    pub cmds: &'static [&'static str],
    pub args: &'static [&'static str],
    pub install: &'static str,
}

const RUST: ServerCfg = ServerCfg {
    key: "rust",
    cmds: &["rust-analyzer"],
    args: &[],
    install: "sudo pacman -S rust-analyzer",
};
const PYTHON: ServerCfg = ServerCfg {
    key: "python",
    cmds: &["pyright-langserver", "basedpyright-langserver"],
    args: &["--stdio"],
    install: "sudo pacman -S pyright",
};
const GO: ServerCfg = ServerCfg { key: "go", cmds: &["gopls"], args: &[], install: "sudo pacman -S gopls" };
const TS: ServerCfg = ServerCfg {
    key: "ts",
    cmds: &["typescript-language-server"],
    args: &["--stdio"],
    install: "sudo pacman -S typescript-language-server",
};

/// Server config and LSP language id for a file.
pub fn server_for(path: &Path) -> Option<(ServerCfg, &'static str)> {
    Some(match path.extension()?.to_str()? {
        "rs" => (RUST, "rust"),
        "py" | "pyi" => (PYTHON, "python"),
        "go" => (GO, "go"),
        "ts" | "mts" | "cts" => (TS, "typescript"),
        "tsx" => (TS, "typescriptreact"),
        "js" | "mjs" | "cjs" => (TS, "javascript"),
        "jsx" => (TS, "javascriptreact"),
        _ => return None,
    })
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

#[derive(Default)]
struct State {
    diagnostics: HashMap<String, Vec<Value>>,
    progress: HashSet<String>,
    seen_progress: bool,
}

type Pending = Arc<Mutex<HashMap<i64, Sender<Result<Value, String>>>>>;

pub struct Client {
    pub name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Pending,
    state: Arc<Mutex<State>>,
    /// uri -> (version, content hash)
    docs: Mutex<HashMap<String, (i64, u64)>>,
    /// Server agreed to UTF-8 columns (else UTF-16).
    pub utf8: bool,
    started: Instant,
}

fn reader_loop(stdout: impl Read, stdin: Arc<Mutex<ChildStdin>>, pending: Pending, state: Arc<Mutex<State>>) {
    let mut r = BufReader::new(stdout);
    while let Ok(Some(msg)) = read_msg(&mut r) {
        let method = msg.get("method").and_then(Value::as_str);
        match (method, msg.get("id")) {
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
                    let res = match msg.get("error") {
                        Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(res);
                }
            }
            (Some(m), Some(id)) => {
                // Requests from the server: answer with harmless defaults.
                let result = match m {
                    "workspace/configuration" => {
                        let n = msg.pointer("/params/items").and_then(Value::as_array).map_or(1, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    }
                    "workspace/applyEdit" => json!({"applied": false}),
                    _ => Value::Null,
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let _ = write_msg(&mut *stdin.lock().unwrap(), &reply);
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                if let Some(uri) = msg.pointer("/params/uri").and_then(Value::as_str) {
                    let diags = msg.pointer("/params/diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
                    state.lock().unwrap().diagnostics.insert(uri.to_string(), diags);
                }
            }
            (Some("$/progress"), None) => {
                let token = msg.pointer("/params/token").map(|t| t.to_string()).unwrap_or_default();
                let kind = msg.pointer("/params/value/kind").and_then(Value::as_str);
                let mut st = state.lock().unwrap();
                st.seen_progress = true;
                match kind {
                    Some("begin") => {
                        st.progress.insert(token);
                    }
                    Some("end") => {
                        st.progress.remove(&token);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err("language server exited".into()));
    }
}

impl Client {
    pub fn start(cfg: &ServerCfg, root: &Path) -> Result<Client> {
        let exe = cfg
            .cmds
            .iter()
            .find_map(|c| which(c))
            .ok_or_else(|| anyhow!("no {} language server found (tried {}). Install it: {}", cfg.key, cfg.cmds.join(", "), cfg.install))?;
        let mut child = Command::new(&exe)
            .args(cfg.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let pending: Pending = Arc::default();
        let state: Arc<Mutex<State>> = Arc::default();
        {
            let (stdin, pending, state) = (stdin.clone(), pending.clone(), state.clone());
            std::thread::spawn(move || reader_loop(stdout, stdin, pending, state));
        }
        let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut client = Client {
            name,
            stdin,
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending,
            state,
            docs: Mutex::default(),
            utf8: false,
            started: Instant::now(),
        };
        let root_uri = path_to_uri(root);
        let folder = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let init = client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": root,
                "workspaceFolders": [{"uri": root_uri, "name": folder}],
                "clientInfo": {"name": "jack", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {
                    "general": {"positionEncodings": ["utf-8", "utf-16"]},
                    "window": {"workDoneProgress": true},
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "workspaceEdit": {"documentChanges": true},
                        "symbol": {}
                    },
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "definition": {"linkSupport": true},
                        "references": {},
                        "rename": {"prepareSupport": false},
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "publishDiagnostics": {"relatedInformation": false}
                    }
                }
            }),
            Duration::from_secs(60),
        )?;
        client.utf8 = init.pointer("/capabilities/positionEncoding").and_then(Value::as_str) == Some("utf-8");
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        write_msg(&mut *self.stdin.lock().unwrap(), &msg).with_context(|| format!("{}: sending {method}", self.name))
    }

    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write_msg(&mut *self.stdin.lock().unwrap(), &msg).with_context(|| format!("{}: sending {method}", self.name))?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("{}: {method}: {e}", self.name),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("{}: {method} timed out after {}s", self.name, timeout.as_secs())
            }
        }
    }

    /// Wait until the server reports no work in progress (indexing etc.),
    /// or `max` passes.
    pub fn wait_ready(&self, max: Duration) {
        let start = Instant::now();
        let mut idle_since: Option<Instant> = None;
        while start.elapsed() < max {
            let (busy, seen) = {
                let st = self.state.lock().unwrap();
                (!st.progress.is_empty(), st.seen_progress)
            };
            // Before any progress has been seen, give the server a moment to start some.
            let settle = if seen { 300 } else { 1500 };
            if busy {
                idle_since = None;
            } else if idle_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(settle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Make sure the server has the current contents of `path`. Returns the text.
    pub fn sync(&self, path: &Path, language_id: &str) -> Result<String> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let uri = path_to_uri(path);
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        let mut docs = self.docs.lock().unwrap();
        match docs.get_mut(&uri) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
                )?;
                docs.insert(uri, (1, hash));
            }
            Some((version, old)) if *old != hash => {
                *version += 1;
                *old = hash;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": *version}, "contentChanges": [{"text": text}]}),
                )?;
            }
            _ => {}
        }
        Ok(text)
    }

    fn position_params(&self, path: &Path, text: &str, row: usize, byte_col: usize) -> Value {
        json!({
            "textDocument": {"uri": path_to_uri(path)},
            "position": {"line": row, "character": byte_to_lsp_col(line_of(text, row), byte_col, self.utf8)}
        })
    }

    /// Run a location query, retrying while a young server is still indexing
    /// (servers answer empty or "content modified" until they are ready).
    fn query_locations(&self, method: &str, params: Value) -> Result<Vec<Loc>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let res = self.request(method, params.clone(), Duration::from_secs(30));
            let young = self.started.elapsed() < Duration::from_secs(180);
            match res {
                Ok(v) => {
                    let locs = parse_locations(&v);
                    if !locs.is_empty() || !young || Instant::now() > deadline {
                        return Ok(locs);
                    }
                }
                Err(e) if !young || Instant::now() > deadline => return Err(e),
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(700));
            self.wait_ready(Duration::from_secs(10));
        }
    }

    pub fn definition(&self, path: &Path, lang_id: &str, row: usize, byte_col: usize) -> Result<Vec<Loc>> {
        let text = self.sync(path, lang_id)?;
        self.query_locations("textDocument/definition", self.position_params(path, &text, row, byte_col))
    }

    pub fn references(&self, path: &Path, lang_id: &str, row: usize, byte_col: usize) -> Result<Vec<Loc>> {
        let text = self.sync(path, lang_id)?;
        let mut params = self.position_params(path, &text, row, byte_col);
        params["context"] = json!({"includeDeclaration": true});
        self.query_locations("textDocument/references", params)
    }

    /// Returns the server's `WorkspaceEdit` (not applied).
    pub fn rename(&self, path: &Path, lang_id: &str, row: usize, byte_col: usize, new_name: &str) -> Result<Value> {
        let text = self.sync(path, lang_id)?;
        let mut params = self.position_params(path, &text, row, byte_col);
        params["newName"] = json!(new_name);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.request("textDocument/rename", params.clone(), Duration::from_secs(30)) {
                Ok(v) if !v.is_null() => return Ok(v),
                Ok(_) if Instant::now() > deadline => bail!("{}: rename returned nothing", self.name),
                Err(e) if Instant::now() > deadline || self.started.elapsed() > Duration::from_secs(180) => return Err(e),
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(700));
            self.wait_ready(Duration::from_secs(10));
        }
    }

    /// Current diagnostics for `path`, waiting up to `max` for the first report.
    pub fn diagnostics(&self, path: &Path, lang_id: &str, max: Duration) -> Result<Vec<Value>> {
        self.sync(path, lang_id)?;
        let uri = path_to_uri(path);
        // Some servers (rust-analyzer's cargo check) only run full checks on save.
        self.notify("textDocument/didSave", json!({"textDocument": {"uri": uri}}))?;
        let start = Instant::now();
        loop {
            if let Some(d) = self.state.lock().unwrap().diagnostics.get(&uri) {
                // Servers often send an empty set first, then the real one.
                if !d.is_empty() || start.elapsed() > max / 2 {
                    return Ok(d.clone());
                }
            }
            if start.elapsed() > max {
                return Ok(vec![]);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.request("shutdown", Value::Null, Duration::from_secs(2));
        let _ = self.notify("exit", Value::Null);
        let mut child = self.child.lock().unwrap();
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------- pool

/// Language servers for one jack session, started on first use.
pub struct Lsp {
    root: PathBuf,
    clients: Mutex<HashMap<&'static str, Arc<Client>>>,
}

impl Lsp {
    pub fn new(root: PathBuf) -> Lsp {
        Lsp { root, clients: Mutex::default() }
    }

    /// The client for a file's language, plus the LSP language id.
    pub fn client_for(&self, path: &Path) -> Result<(Arc<Client>, &'static str)> {
        let (cfg, lang_id) = server_for(path).ok_or_else(|| anyhow!("no language server for {}", path.display()))?;
        let mut clients = self.clients.lock().unwrap();
        if let Some(c) = clients.get(cfg.key) {
            return Ok((c.clone(), lang_id));
        }
        let c = Arc::new(Client::start(&cfg, &self.root)?);
        c.wait_ready(Duration::from_secs(90));
        clients.insert(cfg.key, c.clone());
        Ok((c, lang_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &json!({"a": "héllo"})).unwrap();
        write_msg(&mut buf, &json!({"b": 2})).unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_msg(&mut r).unwrap().unwrap(), json!({"a": "héllo"}));
        assert_eq!(read_msg(&mut r).unwrap().unwrap(), json!({"b": 2}));
        assert!(read_msg(&mut r).unwrap().is_none());
    }

    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/tmp/a dir/ü#x.rs");
        let u = path_to_uri(p);
        assert_eq!(u, "file:///tmp/a%20dir/%C3%BC%23x.rs");
        assert_eq!(uri_to_path(&u).unwrap(), p);
        assert_eq!(uri_to_path("file:///x/%2").unwrap(), PathBuf::from("/x/%2"));
    }

    #[test]
    fn columns_utf16_vs_utf8() {
        let line = "let é😀 = foo;";
        let foo = line.find("foo").unwrap();
        assert_eq!(byte_to_lsp_col(line, foo, true), foo);
        // "let " (4) + é (1) + 😀 (2, a surrogate pair) + " = " (3) = 10 UTF-16 units.
        let u16 = byte_to_lsp_col(line, foo, false);
        assert_eq!(u16, 10);
        assert_eq!(lsp_to_byte_col(line, u16, false), foo);
    }

    #[test]
    fn text_edits_apply_in_reverse() {
        let text = "fn old() {}\nfn main() { old(); old(); }\n";
        let e = |l, s, t, n: &str| json!({"range": {"start": {"line": l, "character": s}, "end": {"line": l, "character": t}}, "newText": n});
        let edits = vec![e(0, 3, 6, "new"), e(1, 12, 15, "new"), e(1, 19, 22, "new")];
        assert_eq!(apply_text_edits(text, &edits, true).unwrap(), "fn new() {}\nfn main() { new(); new(); }\n");
        let overlap = vec![e(0, 0, 5, "x"), e(0, 3, 6, "y")];
        assert!(apply_text_edits(text, &overlap, true).is_err());
    }

    #[test]
    fn workspace_edit_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(&a, "old\n").unwrap();
        let edit = json!({"documentChanges": [{
            "textDocument": {"uri": path_to_uri(&a), "version": 1},
            "edits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]
        }]});
        let done = apply_workspace_edit(&edit, false).unwrap();
        assert_eq!(done, vec![(a.clone(), 1)]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "new\n");
        let bad = json!({"documentChanges": [{"kind": "rename", "oldUri": "file:///a", "newUri": "file:///b"}]});
        assert!(apply_workspace_edit(&bad, false).is_err());
    }

    #[test]
    fn locations_and_links() {
        let loc = json!({"uri": "file:///a.rs", "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 7}}});
        let link = json!([{"targetUri": "file:///b.rs", "targetRange": {}, "targetSelectionRange": {"start": {"line": 9, "character": 1}, "end": {}}}]);
        assert_eq!(parse_locations(&loc), vec![Loc { path: "/a.rs".into(), row: 3, character: 4 }]);
        assert_eq!(parse_locations(&link)[0].row, 9);
        assert!(parse_locations(&Value::Null).is_empty());
    }
}
