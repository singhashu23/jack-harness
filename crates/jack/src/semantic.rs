//! Semantic tools backed by language servers: definition, references,
//! rename and diagnostics. The model names a symbol and a file; jack finds
//! the exact position (tree-sitter outline, or a whole-word match on a given
//! line) and asks the language server. Results are shaped like every other
//! jack tool: grouped, capped, and with the code that matters inline.

use crate::tools::Tools;
use anyhow::{Result, anyhow, bail};
use jack_lsp::{Client, Loc, apply_workspace_edit, server_for, workspace_edit_files};
use jack_read::{Lang, find_symbols, number_lines, outline};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte column of `word` in `line`, matched as a whole identifier.
fn find_word(line: &str, word: &str) -> Option<usize> {
    if word.is_empty() {
        return None;
    }
    line.match_indices(word).map(|(i, _)| i).find(|&i| {
        let before = line[..i].chars().next_back().is_none_or(|c| !is_ident(c));
        let after = line[i + word.len()..].chars().next().is_none_or(|c| !is_ident(c));
        before && after
    })
}

/// `Store::open` -> `open`, `A.m` -> `m`.
fn short_name(symbol: &str) -> &str {
    let s = symbol.rsplit("::").next().unwrap_or(symbol);
    s.rsplit('.').next().unwrap_or(s)
}

/// Where a query points, plus the server to ask.
struct Aim {
    symbol: String,
    path: PathBuf,
    shown: String,
    row: usize,
    col: usize,
    client: Arc<Client>,
    lang_id: &'static str,
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

impl Tools {
    fn display(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
    }

    /// Find the position of `symbol` in `src`: on `line` if given, else its
    /// definition in the outline, else its first whole-word occurrence.
    fn locate(&self, path: &Path, shown: &str, src: &str, symbol: &str, line: Option<usize>) -> Result<(usize, usize)> {
        let short = short_name(symbol);
        if let Some(l) = line {
            let text = src.split('\n').nth(l.saturating_sub(1)).ok_or_else(|| anyhow!("{shown} has no line {l}"))?;
            let col = find_word(text, short).ok_or_else(|| anyhow!("`{short}` is not on line {l} of {shown}"))?;
            return Ok((l - 1, col));
        }
        if let Some(lang) = Lang::from_path(path)
            && let Some(s) = find_symbols(&outline(src, lang), symbol).first()
        {
            return Ok(s.name_pos);
        }
        src.split('\n')
            .enumerate()
            .find_map(|(row, t)| find_word(t, short).map(|c| (row, c)))
            .ok_or_else(|| anyhow!("`{symbol}` not found in {shown}"))
    }

    fn aim(&self, args: &Value) -> Result<Aim> {
        let symbol = arg_str(args, "symbol").ok_or_else(|| anyhow!("missing `symbol`"))?;
        let p = arg_str(args, "path").ok_or_else(|| anyhow!("missing `path` (a file where `symbol` appears)"))?;
        let line = args.get("line").and_then(Value::as_u64).map(|l| l as usize);
        let (path, shown) = self.resolve(p)?;
        let src = std::fs::read_to_string(&path)?;
        let (row, col) = self.locate(&path, &shown, &src, symbol, line)?;
        let (client, lang_id) = self.lsp.client_for(&path)?;
        Ok(Aim { symbol: symbol.to_string(), path, shown, row, col, client, lang_id })
    }

    pub(crate) fn jack_def(&self, args: &Value) -> Result<(String, usize)> {
        let a = self.aim(args)?;
        let locs = a.client.definition(&a.path, a.lang_id, a.row, a.col)?;
        if locs.is_empty() {
            bail!("{}: no definition found for `{}` at {}:{}", a.client.name, a.symbol, a.shown, a.row + 1);
        }
        let mut out = String::new();
        let mut raw = 0;
        for loc in locs.iter().take(3) {
            let src = std::fs::read_to_string(&loc.path).unwrap_or_default();
            raw += src.len();
            let shown = self.display(&loc.path);
            let line_no = loc.row + 1;
            // The innermost outline symbol containing the definition line.
            let body = Lang::from_path(&loc.path).and_then(|lang| {
                outline(&src, lang)
                    .into_iter()
                    .filter(|s| s.start_line <= line_no && line_no <= s.end_line)
                    .min_by_key(|s| s.end_line - s.start_line)
            });
            match body {
                Some(s) => {
                    let end = s.end_line.min(s.start_line + 79);
                    out.push_str(&format!("{shown}:{line_no}, {} {} L{}-{}:\n", s.kind, s.name, s.start_line, s.end_line));
                    out.push_str(&number_lines(&src, s.start_line, end));
                    if end < s.end_line {
                        out.push_str(&format!("[cut; jack_read lines={}-{} for the rest]\n", end + 1, s.end_line));
                    }
                }
                None => {
                    out.push_str(&format!("{shown}:{line_no}:\n"));
                    out.push_str(&number_lines(&src, line_no.saturating_sub(2).max(1), line_no + 8));
                }
            }
        }
        if locs.len() > 3 {
            out.push_str(&format!("[{} more definitions not shown]\n", locs.len() - 3));
        }
        Ok((out, raw))
    }

    pub(crate) fn jack_refs(&self, args: &Value) -> Result<(String, usize)> {
        let a = self.aim(args)?;
        let locs = a.client.references(&a.path, a.lang_id, a.row, a.col)?;
        if locs.is_empty() {
            return Ok((format!("no references to `{}` found ({})", a.symbol, a.client.name), 0));
        }
        let max = args.get("max").and_then(Value::as_u64).unwrap_or(60) as usize;
        let mut files: Vec<(PathBuf, Vec<&Loc>)> = Vec::new();
        for l in &locs {
            match files.iter_mut().find(|(p, _)| *p == l.path) {
                Some((_, v)) => v.push(l),
                None => files.push((l.path.clone(), vec![l])),
            }
        }
        let mut out = format!("{} references to `{}` in {} files:\n", locs.len(), a.symbol, files.len());
        let mut shown_n = 0;
        let mut truncated = false;
        for (p, v) in &files {
            if shown_n >= max {
                truncated = true;
                break;
            }
            let src = std::fs::read_to_string(p).unwrap_or_default();
            let lines: Vec<&str> = src.lines().collect();
            out.push_str(&self.display(p));
            out.push('\n');
            let mut rows: Vec<usize> = v.iter().map(|l| l.row).collect();
            rows.sort_unstable();
            rows.dedup();
            for row in rows {
                if shown_n >= max {
                    truncated = true;
                    break;
                }
                let t: String = lines.get(row).map(|s| s.trim()).unwrap_or("").chars().take(160).collect();
                out.push_str(&format!("  {}: {t}\n", row + 1));
                shown_n += 1;
            }
        }
        if truncated {
            out.push_str(&format!("[showing {shown_n}; pass max= to see more]\n"));
        }
        Ok((out, 0))
    }

    pub(crate) fn jack_rename(&self, args: &Value) -> Result<(String, usize)> {
        let new_name = arg_str(args, "new_name").ok_or_else(|| anyhow!("missing `new_name`"))?;
        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
        let a = self.aim(args)?;
        let edit = a.client.rename(&a.path, a.lang_id, a.row, a.col, new_name)?;
        let files = if dry_run { workspace_edit_files(&edit) } else { apply_workspace_edit(&edit, a.client.utf8)? };
        if files.is_empty() {
            bail!("{}: rename produced no edits", a.client.name);
        }
        if !dry_run {
            // Give the server the new contents so later queries see them.
            for (p, _) in &files {
                let lang_id = server_for(p).map_or(a.lang_id, |(_, id)| id);
                let _ = a.client.sync(p, lang_id);
            }
        }
        let total: usize = files.iter().map(|(_, n)| n).sum();
        let mut out = format!(
            "{} `{}` to `{new_name}`: {total} edits in {} files\n",
            if dry_run { "would rename" } else { "renamed" },
            a.symbol,
            files.len()
        );
        for (p, n) in &files {
            out.push_str(&format!("  {} ({n})\n", self.display(p)));
        }
        if !dry_run {
            out.push_str("Re-read changed files with jack_read to see the diffs.\n");
        }
        Ok((out, 0))
    }

    pub(crate) fn jack_diagnostics(&self, args: &Value) -> Result<(String, usize)> {
        let p = arg_str(args, "path").ok_or_else(|| anyhow!("missing `path`"))?;
        let (path, shown) = self.resolve(p)?;
        let (client, lang_id) = self.lsp.client_for(&path)?;
        let diags = client.diagnostics(&path, lang_id, Duration::from_secs(10))?;
        let sev = |d: &Value| d["severity"].as_u64().unwrap_or(1);
        let line = |d: &Value| d.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(0);
        let mut items: Vec<&Value> = diags.iter().filter(|d| sev(d) <= 2).collect();
        items.sort_by_key(|d| (sev(d), line(d)));
        let errors = items.iter().filter(|d| sev(d) == 1).count();
        let warnings = items.len() - errors;
        let minor = diags.len() - items.len();
        if items.is_empty() {
            return Ok((format!("{shown}: no errors or warnings ({}; {minor} hints/infos)", client.name), 0));
        }
        let mut out = format!("{shown}: {errors} errors, {warnings} warnings ({}):\n", client.name);
        for d in items.iter().take(40) {
            let msg = d["message"].as_str().unwrap_or("").lines().next().unwrap_or("");
            let src = d["source"].as_str().map(|s| format!(" [{s}]")).unwrap_or_default();
            let kind = if sev(d) == 1 { "error" } else { "warning" };
            out.push_str(&format!("  L{}: {kind}: {msg}{src}\n", line(d) + 1));
        }
        if items.len() > 40 {
            out.push_str(&format!("[{} more not shown]\n", items.len() - 40));
        }
        Ok((out, 0))
    }
}

pub fn semantic_tools() -> Vec<Value> {
    let aim_props = |extra: Value| {
        let mut props = json!({
            "symbol": {"type": "string", "description": "Symbol name, e.g. 'verify_token', 'Store::open', 'MyClass.method'"},
            "path": {"type": "string", "description": "A file where the symbol appears (its definition or a usage)"},
            "line": {"type": "integer", "description": "1-based line in `path` where the symbol appears; use it to point at a specific usage"}
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        props
    };
    vec![
        json!({
            "name": "jack_def",
            "description": "Go to a symbol's definition using the language server (type-aware, follows imports across files). Returns the definition's code, so no separate file read is needed.",
            "inputSchema": {"type": "object", "properties": aim_props(json!({})), "required": ["symbol", "path"], "additionalProperties": false}
        }),
        json!({
            "name": "jack_refs",
            "description": "Find every reference to a symbol using the language server: exact, not a text search. Results are grouped by file with each line's code.",
            "inputSchema": {"type": "object", "properties": aim_props(json!({"max": {"type": "integer", "description": "Lines to show, default 60"}})), "required": ["symbol", "path"], "additionalProperties": false}
        }),
        json!({
            "name": "jack_rename",
            "description": "Rename a symbol everywhere it is used, via the language server, and write the changes to disk. Use dry_run=true to only list the files it would change.",
            "inputSchema": {"type": "object", "properties": aim_props(json!({
                "new_name": {"type": "string"},
                "dry_run": {"type": "boolean"}
            })), "required": ["symbol", "path", "new_name"], "additionalProperties": false}
        }),
        json!({
            "name": "jack_diagnostics",
            "description": "Errors and warnings for a file from its language server (for Rust this includes cargo check). Faster and much shorter than running a full build.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use jack_store::Store;

    fn on_path(cmd: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
    }

    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Tools) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let root = dir.path().canonicalize().unwrap();
        (dir, Tools::new(Store::open_in_memory().unwrap(), root))
    }

    fn ok(t: &Tools, tool: &str, args: Value) -> String {
        let o = t.call(tool, &args);
        assert!(!o.is_error, "{tool}: {}", o.text);
        o.text
    }

    /// Live test against rust-analyzer; skipped when it isn't installed.
    #[test]
    fn rust_analyzer_live() {
        if !on_path("rust-analyzer") {
            eprintln!("skipping: rust-analyzer not installed");
            return;
        }
        let (dir, t) = project(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/util.rs", "pub fn helper(x: i32) -> i32 {\n    x + 1\n}\n"),
            ("src/main.rs", "mod util;\nuse util::helper;\n\nfn main() {\n    let a = helper(1);\n    let b = helper(a);\n    println!(\"{b}\");\n}\n"),
        ]);
        let d = ok(&t, "jack_def", json!({"symbol": "helper", "path": "src/main.rs", "line": 5}));
        assert!(d.contains("src/util.rs:1") && d.contains("x + 1"), "{d}");
        let r = ok(&t, "jack_refs", json!({"symbol": "helper", "path": "src/util.rs"}));
        assert!(r.contains("src/main.rs") && r.contains("helper(a)"), "{r}");
        let n = ok(&t, "jack_rename", json!({"symbol": "helper", "path": "src/util.rs", "new_name": "assist"}));
        assert!(n.contains("renamed"), "{n}");
        let main = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
        assert!(main.contains("assist(1)") && !main.contains("helper"), "{main}");
        std::fs::write(dir.path().join("src/util.rs"), "pub fn assist(x: i32) -> i32 {\n    \"oops\"\n}\n").unwrap();
        let g = ok(&t, "jack_diagnostics", json!({"path": "src/util.rs"}));
        assert!(g.contains("error"), "{g}");
    }

    /// Live test against gopls; skipped when it isn't installed.
    #[test]
    fn gopls_live() {
        if !on_path("gopls") {
            eprintln!("skipping: gopls not installed");
            return;
        }
        let (_dir, t) = project(&[
            ("go.mod", "module demo\n\ngo 1.21\n"),
            ("util.go", "package main\n\nfunc helper(x int) int {\n\treturn x + 1\n}\n"),
            ("main.go", "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(helper(1) + helper(2))\n}\n"),
        ]);
        let d = ok(&t, "jack_def", json!({"symbol": "helper", "path": "main.go", "line": 6}));
        assert!(d.contains("util.go:3") && d.contains("return x + 1"), "{d}");
        let r = ok(&t, "jack_refs", json!({"symbol": "helper", "path": "util.go"}));
        assert!(r.contains("3 references") && r.contains("main.go"), "{r}");
    }

    /// Live test against typescript-language-server; skipped when it isn't installed.
    #[test]
    fn typescript_live() {
        if !on_path("typescript-language-server") {
            eprintln!("skipping: typescript-language-server not installed");
            return;
        }
        let (_dir, t) = project(&[
            ("tsconfig.json", "{\"compilerOptions\": {\"strict\": true}}\n"),
            ("util.ts", "export function helper(x: number): number {\n  return x + 1;\n}\n"),
            ("main.ts", "import { helper } from \"./util\";\n\nconsole.log(helper(1) + helper(2));\n"),
        ]);
        // References first, on a cold server: the project must be loaded
        // before answering, or usages in unopened files are missed.
        let r = ok(&t, "jack_refs", json!({"symbol": "helper", "path": "util.ts"}));
        assert!(r.contains("main.ts") && r.contains("console.log(helper(1)"), "{r}");
        // From a usage, the definition must follow the import to util.ts.
        let d = ok(&t, "jack_def", json!({"symbol": "helper", "path": "main.ts", "line": 3}));
        assert!(d.contains("util.ts:1") && d.contains("return x + 1"), "{d}");
    }

    /// Live test against pyright; skipped when it isn't installed.
    #[test]
    fn pyright_live() {
        if !on_path("pyright-langserver") && !on_path("basedpyright-langserver") {
            eprintln!("skipping: pyright not installed");
            return;
        }
        let (_dir, t) = project(&[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "def helper(x: int) -> int:\n    return x + 1\n"),
            ("main.py", "from pkg.a import helper\n\nprint(helper(1) + helper(2))\n"),
        ]);
        let d = ok(&t, "jack_def", json!({"symbol": "helper", "path": "main.py", "line": 3}));
        assert!(d.contains("pkg/a.py:1") && d.contains("return x + 1"), "{d}");
        let r = ok(&t, "jack_refs", json!({"symbol": "helper", "path": "pkg/a.py"}));
        assert!(r.contains("main.py"), "{r}");
    }

    #[test]
    fn whole_word_matching() {
        assert_eq!(find_word("let open_all = open(x);", "open"), Some(15));
        assert_eq!(find_word("reopen()", "open"), None);
        assert_eq!(find_word("x.open", "open"), Some(2));
        assert_eq!(short_name("Store::open"), "open");
        assert_eq!(short_name("A.m"), "m");
    }
}
