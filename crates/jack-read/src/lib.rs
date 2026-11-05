//! Code-reading primitives for jack: tree-sitter outlines, symbol lookup,
//! line slicing, diffs and output trimming. Pure functions, no I/O state;
//! the session-aware policy (unchanged / diff / outline) lives in the `jack`
//! crate, which combines these with `jack-store`.

use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        match path.extension()?.to_str()? {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
        }
    }

    fn grammar(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Short label: fn, struct, class, def, method, impl, ...
    pub kind: &'static str,
    pub name: String,
    /// Name of the enclosing container (impl type, class, module), if any.
    pub parent: Option<String>,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub depth: usize,
}

/// What a node means for the outline.
enum Role {
    /// A symbol whose body is not descended into (functions).
    Leaf(&'static str),
    /// A symbol that contains other symbols (classes, impls, modules).
    Container(&'static str),
    /// Not a symbol; keep looking inside it.
    Pass,
}

fn role(lang: Lang, kind: &str) -> Role {
    use Role::*;
    match (lang, kind) {
        (Lang::Rust, "function_item") => Leaf("fn"),
        (Lang::Rust, "function_signature_item") => Leaf("fn"),
        (Lang::Rust, "struct_item") => Leaf("struct"),
        (Lang::Rust, "enum_item") => Leaf("enum"),
        (Lang::Rust, "union_item") => Leaf("union"),
        (Lang::Rust, "type_item") => Leaf("type"),
        (Lang::Rust, "const_item") => Leaf("const"),
        (Lang::Rust, "static_item") => Leaf("static"),
        (Lang::Rust, "macro_definition") => Leaf("macro"),
        (Lang::Rust, "trait_item") => Container("trait"),
        (Lang::Rust, "impl_item") => Container("impl"),
        (Lang::Rust, "mod_item") => Container("mod"),

        (Lang::Python, "function_definition") => Leaf("def"),
        (Lang::Python, "class_definition") => Container("class"),

        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, k) => match k {
            "function_declaration" | "generator_function_declaration" => Leaf("function"),
            "method_definition" | "method_signature" | "abstract_method_signature" => Leaf("method"),
            "interface_declaration" => Leaf("interface"),
            "type_alias_declaration" => Leaf("type"),
            "enum_declaration" => Leaf("enum"),
            "class_declaration" | "abstract_class_declaration" | "class" => Container("class"),
            "internal_module" | "module" => Container("namespace"),
            _ => Pass,
        },

        (Lang::Go, "function_declaration") => Leaf("func"),
        (Lang::Go, "method_declaration") => Leaf("method"),
        (Lang::Go, "type_spec") => Leaf("type"),
        (Lang::Go, "const_spec") => Leaf("const"),
        _ => Pass,
    }
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// The display name for a symbol node.
fn symbol_name(lang: Lang, node: Node, src: &[u8]) -> Option<String> {
    match (lang, node.kind()) {
        (Lang::Rust, "impl_item") => {
            let ty = text(node.child_by_field_name("type")?, src);
            Some(match node.child_by_field_name("trait") {
                Some(tr) => format!("{} for {}", text(tr, src), ty),
                None => ty.to_string(),
            })
        }
        (Lang::Go, "method_declaration") => {
            let name = text(node.child_by_field_name("name")?, src);
            let recv = node
                .child_by_field_name("receiver")
                .map(|r| text(r, src).to_string())
                .unwrap_or_default();
            Some(format!("{recv} {name}").trim().to_string())
        }
        _ => node.child_by_field_name("name").map(|n| text(n, src).to_string()),
    }
}

/// JS/TS `const foo = () => {}` and `const foo = function () {}`.
fn arrow_binding(lang: Lang, node: Node, src: &[u8]) -> Option<String> {
    if !matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
        || node.kind() != "variable_declarator"
    {
        return None;
    }
    let value = node.child_by_field_name("value")?;
    if matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
        Some(text(node.child_by_field_name("name")?, src).to_string())
    } else {
        None
    }
}

fn walk(
    lang: Lang,
    node: Node,
    src: &[u8],
    depth: usize,
    parent: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let push = |out: &mut Vec<Symbol>, kind, name: String| {
            out.push(Symbol {
                kind,
                name,
                parent: parent.map(str::to_string),
                start_line: child.start_position().row + 1,
                end_line: child.end_position().row + 1,
                depth,
            })
        };
        if let Some(name) = arrow_binding(lang, child, src) {
            push(out, "function", name);
            continue;
        }
        match role(lang, child.kind()) {
            Role::Leaf(kind) => {
                // Methods inside a class read better as "method".
                let kind = if kind == "def" && parent.is_some() { "method" } else { kind };
                if let Some(name) = symbol_name(lang, child, src) {
                    push(out, kind, name);
                }
            }
            Role::Container(kind) => {
                let name = symbol_name(lang, child, src).unwrap_or_else(|| "<anonymous>".into());
                push(out, kind, name.clone());
                walk(lang, child, src, depth + 1, Some(&name), out);
            }
            Role::Pass => walk(lang, child, src, depth, parent, out),
        }
    }
}

/// Extract the symbol outline of a source file.
pub fn outline(src: &str, lang: Lang) -> Vec<Symbol> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(src, None) else {
        return vec![];
    };
    let mut out = Vec::new();
    walk(lang, tree.root_node(), src.as_bytes(), 0, None, &mut out);
    out
}

/// Render an outline compactly: one line per symbol with its line range.
pub fn render_outline(path: &str, lang: Lang, total_lines: usize, syms: &[Symbol]) -> String {
    let mut s = format!("{path} ({total_lines} lines, {}) outline:\n", lang.name());
    for sym in syms {
        s.push_str(&format!(
            "{}{} {}  L{}-{}\n",
            "  ".repeat(sym.depth + 1),
            sym.kind,
            sym.name,
            sym.start_line,
            sym.end_line
        ));
    }
    if syms.is_empty() {
        s.push_str("  (no symbols found)\n");
    }
    s
}

/// Find symbols by name. Accepts a bare name (`verify`) or a qualified one
/// (`Store::open`, `Store.open`).
pub fn find_symbols<'a>(syms: &'a [Symbol], query: &str) -> Vec<&'a Symbol> {
    let (want_parent, want_name) = match query.rsplit_once("::").or_else(|| query.rsplit_once('.')) {
        Some((p, n)) => (Some(p), n),
        None => (None, query),
    };
    syms.iter()
        .filter(|s| {
            let short = s.name.rsplit(' ').next().unwrap_or(&s.name);
            (s.name == want_name || short == want_name)
                && want_parent.is_none_or(|p| s.parent.as_deref() == Some(p))
        })
        .collect()
}

/// Lines `start..=end` (1-based) prefixed with their line numbers.
pub fn number_lines(src: &str, start: usize, end: usize) -> String {
    let mut s = String::new();
    for (i, line) in src.lines().enumerate() {
        let n = i + 1;
        if n < start {
            continue;
        }
        if n > end {
            break;
        }
        s.push_str(&format!("{n}\t{line}\n"));
    }
    s
}

/// Unified diff between two versions of a file, 3 lines of context.
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Rough token estimate (about 4 bytes per token for code and logs).
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

fn looks_important(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["error", "fail", "panic", "warning", "exception", "traceback", "fatal", "assert"]
        .iter()
        .any(|k| l.contains(k))
}

/// Trim long command output to head + important lines + tail.
/// Returns the text to show and whether anything was cut.
pub fn trim_output(text: &str, max_lines: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return (text.to_string(), false);
    }
    let head = max_lines / 3;
    let tail = max_lines / 3;
    let middle_budget = max_lines - head - tail;
    let mid_range = head..lines.len() - tail;
    let important: Vec<usize> = mid_range.clone().filter(|&i| looks_important(lines[i])).collect();

    let mut out = String::new();
    for l in &lines[..head] {
        out.push_str(l);
        out.push('\n');
    }
    let shown: Vec<usize> = important.iter().copied().take(middle_budget).collect();
    let mut last = head;
    for &i in &shown {
        if i > last {
            out.push_str(&format!("[... {} lines ...]\n", i - last));
        }
        out.push_str(&format!("{}\t{}\n", i + 1, lines[i]));
        last = i + 1;
    }
    let tail_start = lines.len() - tail;
    if tail_start > last {
        out.push_str(&format!("[... {} lines ...]\n", tail_start - last));
    }
    for l in &lines[tail_start..] {
        out.push_str(l);
        out.push('\n');
    }
    if important.len() > shown.len() {
        out.push_str(&format!(
            "[{} more error/warning lines not shown]\n",
            important.len() - shown.len()
        ));
    }
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = r#"
pub struct Store { conn: u8 }

impl Store {
    pub fn open() -> Self { Store { conn: 0 } }
    fn helper(&self) {}
}

impl Drop for Store {
    fn drop(&mut self) {}
}

mod inner {
    pub fn deep() {}
}

fn main() {
    fn nested_is_hidden() {}
}
"#;

    #[test]
    fn rust_outline() {
        let syms = outline(RUST, Lang::Rust);
        let names: Vec<String> = syms.iter().map(|s| format!("{} {}", s.kind, s.name)).collect();
        assert_eq!(
            names,
            vec![
                "struct Store",
                "impl Store",
                "fn open",
                "fn helper",
                "impl Drop for Store",
                "fn drop",
                "mod inner",
                "fn deep",
                "fn main"
            ]
        );
        let open = find_symbols(&syms, "Store::open");
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].start_line, open[0].end_line), (5, 5));
        assert_eq!(find_symbols(&syms, "drop").len(), 1);
    }

    #[test]
    fn python_outline() {
        let src = "import os\n\nclass A:\n    def m(self):\n        pass\n\n@dec\ndef f():\n    return 1\n";
        let syms = outline(src, Lang::Python);
        let names: Vec<String> = syms.iter().map(|s| format!("{} {}", s.kind, s.name)).collect();
        assert_eq!(names, vec!["class A", "method m", "def f"]);
        assert_eq!(find_symbols(&syms, "A.m")[0].start_line, 4);
    }

    #[test]
    fn ts_outline() {
        let src = "export class Svc {\n  run(): void {}\n}\nexport const go = () => 1;\ninterface I { x: number }\nfunction plain() {}\n";
        let syms = outline(src, Lang::TypeScript);
        let names: Vec<String> = syms.iter().map(|s| format!("{} {}", s.kind, s.name)).collect();
        assert_eq!(names, vec!["class Svc", "method run", "function go", "interface I", "function plain"]);
    }

    #[test]
    fn go_outline() {
        let src = "package m\n\ntype T struct{}\n\nfunc (t *T) Run() {}\n\nfunc New() *T { return nil }\n";
        let syms = outline(src, Lang::Go);
        let names: Vec<String> = syms.iter().map(|s| format!("{} {}", s.kind, s.name)).collect();
        assert_eq!(names, vec!["type T", "method (t *T) Run", "func New"]);
        assert_eq!(find_symbols(&syms, "Run").len(), 1);
    }

    #[test]
    fn numbering_and_diff() {
        assert_eq!(number_lines("a\nb\nc\n", 2, 3), "2\tb\n3\tc\n");
        let d = unified_diff("a\nb\nc\n", "a\nB\nc\n", "x.rs");
        assert!(d.contains("-b") && d.contains("+B"), "{d}");
    }

    #[test]
    fn trim_keeps_errors() {
        let mut log: Vec<String> = (0..300).map(|i| format!("ok line {i}")).collect();
        log[150] = "error[E0308]: mismatched types".into();
        let (out, cut) = trim_output(&log.join("\n"), 30);
        assert!(cut);
        assert!(out.contains("151\terror[E0308]"));
        assert!(out.contains("ok line 0") && out.contains("ok line 299"));
        assert!(out.lines().count() < 40);
    }
}
