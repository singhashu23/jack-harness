mod mcp;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jack_store::Store;
use serde_json::json;
use std::path::PathBuf;
use tools::Tools;

/// jack: a token-efficient coding-agent harness.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server on stdio (one session store per process).
    Mcp {
        /// Project root that relative paths resolve against (default: cwd).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Session id (default: <unix time>-<pid>).
        #[arg(long)]
        session: Option<String>,
    },
    /// Read a file the way the jack_read tool does (CLI session "cli").
    Read {
        path: String,
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long)]
        lines: Option<String>,
        #[arg(long)]
        full: bool,
    },
    /// Print a file's symbol outline.
    Outline { path: PathBuf },
    /// Bytes an unshaped tool would have sent vs. bytes jack sent, per tool.
    Stats {
        /// Only this session (default: all sessions).
        #[arg(long)]
        session: Option<String>,
    },
}

fn jack_home() -> PathBuf {
    std::env::var_os("JACK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".jack"))
}

fn session_store(id: &str) -> Result<Store> {
    Store::open(&jack_home().join("sessions").join(id).join("store.db"))
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Mcp { root, session } => {
            let root = root.unwrap_or(std::env::current_dir()?).canonicalize()?;
            let id = session.unwrap_or_else(|| {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                format!("{t}-{}", std::process::id())
            });
            let store = session_store(&id)?;
            store.log_event("session_start", None, &json!({"root": root, "session": id}), 0, 0)?;
            mcp::serve(Tools::new(store, root))
        }
        Cmd::Read { path, symbol, lines, full } => {
            let tools = Tools::new(session_store("cli")?, std::env::current_dir()?.canonicalize()?);
            let out = tools.call("jack_read", &json!({"path": path, "symbol": symbol, "lines": lines, "full": full}));
            print!("{}", out.text);
            if out.is_error {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Outline { path } => {
            let lang = jack_read::Lang::from_path(&path).context("unsupported file type")?;
            let src = std::fs::read_to_string(&path)?;
            let syms = jack_read::outline(&src, lang);
            print!("{}", jack_read::render_outline(&path.display().to_string(), lang, src.lines().count(), &syms));
            Ok(())
        }
        Cmd::Stats { session } => {
            let dir = jack_home().join("sessions");
            let ids: Vec<String> = match session {
                Some(s) => vec![s],
                None => std::fs::read_dir(&dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default(),
            };
            let mut totals: std::collections::BTreeMap<String, (i64, i64, i64)> = Default::default();
            for id in &ids {
                let path = dir.join(id).join("store.db");
                if !path.exists() {
                    continue;
                }
                for (tool, s) in Store::open(&path)?.stats()? {
                    let t = totals.entry(tool).or_default();
                    t.0 += s.calls;
                    t.1 += s.raw_bytes;
                    t.2 += s.sent_bytes;
                }
            }
            println!("{:<12} {:>7} {:>12} {:>12} {:>7}", "tool", "calls", "raw bytes", "sent bytes", "saved");
            for (tool, (calls, raw, sent)) in &totals {
                let saved = if *raw > 0 { 100.0 * (1.0 - *sent as f64 / *raw as f64) } else { 0.0 };
                println!("{tool:<12} {calls:>7} {raw:>12} {sent:>12} {saved:>6.1}%");
            }
            println!("({} sessions)", ids.len());
            Ok(())
        }
    }
}
