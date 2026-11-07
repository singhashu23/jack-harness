mod mcp;
mod recorder;
mod semantic;
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
    /// Claude Code hook entry points.
    Hook {
        #[command(subcommand)]
        action: HookCmd,
    },
    /// Import Claude Code transcripts (default: all of ~/.claude/projects).
    Import { paths: Vec<PathBuf> },
    /// Recorded Claude Code sessions: tokens, approximate API cost, top tools.
    Sessions {
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// Run a task on an agent CLI headless and report what it did.
    Run {
        /// The task.
        prompt: String,
        /// claude or cursor.
        #[arg(long, default_value = "claude")]
        engine: String,
        /// Working directory (default: cwd).
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        /// Continue an earlier session of the same engine.
        #[arg(long)]
        resume: Option<String>,
        /// read (no edits), edit (edits, no shell) or full (edits and shell).
        #[arg(long, default_value = "edit")]
        access: String,
        /// Attach jack's MCP tools and route file reads through them (claude only for now).
        #[arg(long)]
        jack_tools: bool,
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u64,
        /// Print the full result as JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Past `jack run`s, newest first.
    Runs {
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// One recorded session in detail (id or id prefix).
    Session { id: String },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Record the session in the hook payload (stdin). Always exits 0.
    Record,
}

/// Recorded Claude Code session stores, newest first.
fn recorded_sessions() -> Vec<(String, PathBuf, std::time::SystemTime)> {
    let dir = jack_home().join("sessions");
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_prefix("cc-")?.to_string();
            let db = e.path().join("store.db");
            let t = db.metadata().and_then(|m| m.modified()).ok()?;
            Some((id, db, t))
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.2));
    out
}

fn human(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// Totals over models, plus approximate cost (None if any model is unpriced).
fn session_totals(store: &Store) -> Result<(jack_store::Usage, Option<f64>)> {
    let mut t = jack_store::Usage::default();
    let mut cost = Some(0.0);
    for (model, u) in store.usage_by_model()? {
        t.input += u.input;
        t.output += u.output;
        t.cache_read += u.cache_read;
        t.cache_write += u.cache_write;
        if u.input + u.output + u.cache_read + u.cache_write > 0 {
            cost = match (cost, recorder::cost(&model, &u)) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
        }
    }
    Ok((t, cost))
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
    // Rust ignores SIGPIPE, so `jack sessions | head` would panic on a closed
    // pipe. Restore the default: exit quietly, like other CLI tools.
    // SAFETY: called once at startup, before any other threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        Cmd::Hook { action: HookCmd::Record } => {
            recorder::hook_record(&jack_home());
            Ok(())
        }
        Cmd::Import { paths } => {
            let paths = if paths.is_empty() { recorder::all_transcripts() } else { paths };
            let (mut sessions, mut blocks, mut failed) = (0, 0, 0);
            for p in &paths {
                let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
                match recorder::ingest_session(&jack_home(), p, &id) {
                    Ok(n) => {
                        sessions += 1;
                        blocks += n;
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {e:#}", p.display());
                    }
                }
            }
            println!("imported {sessions} sessions, {blocks} new blocks, {failed} failed");
            Ok(())
        }
        Cmd::Sessions { limit } => {
            println!(
                "{:<10} {:<22} {:>7} {:>6} {:>8} {:>9} {:>9} {:>8} {:>9}",
                "session", "project", "prompts", "tools", "input", "cache rd", "cache wr", "output", "api $"
            );
            for (id, db, _) in recorded_sessions().into_iter().take(limit) {
                let store = Store::open(&db)?;
                let counts: std::collections::HashMap<String, i64> = store.msg_counts()?.into_iter().collect();
                let (t, cost) = session_totals(&store)?;
                let project = store.get_meta("cwd")?.and_then(|c| c.rsplit('/').next().map(str::to_string)).unwrap_or_default();
                println!(
                    "{:<10} {:<22} {:>7} {:>6} {:>8} {:>9} {:>9} {:>8} {:>9}",
                    &id[..id.len().min(8)],
                    project.chars().take(22).collect::<String>(),
                    counts.get("user").copied().unwrap_or(0),
                    counts.get("tool_use").copied().unwrap_or(0),
                    human(t.input),
                    human(t.cache_read),
                    human(t.cache_write),
                    human(t.output),
                    cost.map_or("?".to_string(), |c| format!("{c:.2}")),
                );
            }
            println!("(api $ = approximate pay-as-you-go API price; cache writes at the 5-minute rate)");
            Ok(())
        }
        Cmd::Session { id } => {
            let matches: Vec<_> = recorded_sessions().into_iter().filter(|(s, _, _)| s.starts_with(&id)).collect();
            let (sid, db, _) = match matches.as_slice() {
                [one] => one.clone(),
                [] => anyhow::bail!("no recorded session starts with {id:?}"),
                _ => anyhow::bail!("{} sessions start with {id:?}; give more of the id", matches.len()),
            };
            let store = Store::open(&db)?;
            println!("session {sid}");
            if let Some(c) = store.get_meta("cwd")? {
                println!("project {c}");
            }
            println!("\nmodel usage:");
            for (model, u) in store.usage_by_model()? {
                let c = recorder::cost(&model, &u).map_or("?".to_string(), |c| format!("${c:.2}"));
                println!(
                    "  {model:<20} {:<14} input {:>7}  cache read {:>7}  cache write {:>7}  output {:>7}  {c}",
                    u.msg_id, human(u.input), human(u.cache_read), human(u.cache_write), human(u.output)
                );
            }
            let tools = store.tool_result_bytes()?;
            let total: i64 = tools.iter().map(|t| t.2).sum();
            println!("\ntool results returned into context (~4 bytes per token):");
            for (tool, calls, bytes) in &tools {
                let share = if total > 0 { 100.0 * *bytes as f64 / total as f64 } else { 0.0 };
                println!("  {tool:<28} {calls:>5} calls  ~{:>7} tokens  {share:>5.1}%", human(bytes / 4));
            }
            Ok(())
        }
        Cmd::Run { prompt, engine, cwd, model, resume, access, jack_tools, timeout_secs, json } => {
            use jack_engines::{Access, Engine, Event, RunSpec};
            let engine = Engine::parse(&engine).with_context(|| format!("unknown engine {engine:?} (claude, cursor)"))?;
            jack_engines::check_installed(engine)?;
            let access = match access.as_str() {
                "read" => Access::Read,
                "edit" => Access::Edit,
                "full" => Access::Full,
                a => anyhow::bail!("unknown access {a:?} (read, edit, full)"),
            };
            if jack_tools && engine != Engine::Claude {
                anyhow::bail!("--jack-tools is only wired up for claude so far");
            }
            let cwd = cwd.unwrap_or(std::env::current_dir()?).canonicalize()?;
            let jack_bin = if jack_tools { Some(std::env::current_exe()?) } else { None };
            let spec = RunSpec {
                prompt: prompt.clone(),
                cwd: cwd.clone(),
                resume,
                model,
                access,
                jack_tools: jack_bin,
                timeout: std::time::Duration::from_secs(timeout_secs),
            };
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
            let dir = jack_home().join("runs").join(format!("{stamp}-{}", engine.name()));
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("spec.json"), json!({"engine": engine.name(), "cwd": cwd, "prompt": prompt, "access": access}).to_string())?;
            let r = jack_engines::run(engine, &spec, &dir.join("events.jsonl"), |e| match e {
                Event::Started { session_id, model } => {
                    eprintln!("[{}] session {session_id} ({})", engine.name(), model.as_deref().unwrap_or("?"))
                }
                Event::Tool { name, detail } => eprintln!("  -> {name} {detail}"),
                Event::ToolDone { name, ok: false } => eprintln!("  !! {name} failed"),
                _ => {}
            })?;
            std::fs::write(dir.join("result.json"), serde_json::to_string_pretty(&r)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{} {}", if r.is_error { "FAILED" } else { "done" }, r.engine);
                println!("{}", r.summary.trim());
                if !r.files_changed.is_empty() {
                    println!("\nchanged: {}", r.files_changed.join(", "));
                    if !r.diff_stat.is_empty() {
                        println!("{}", r.diff_stat);
                    }
                }
                let u = &r.usage;
                println!(
                    "\nsession {}  turns {}  tools {}  {:.1}s",
                    r.session_id.as_deref().unwrap_or("?"),
                    r.num_turns.map_or("?".into(), |n| n.to_string()),
                    r.tool_calls.len(),
                    r.duration_ms as f64 / 1000.0
                );
                println!(
                    "tokens: input {}  cache read {}  cache write {}  output {}{}",
                    human(u.input), human(u.cache_read), human(u.cache_write), human(u.output),
                    r.session_cost_usd.map_or(String::new(), |c| format!("  (session api ${c:.3})"))
                );
                println!("log: {}", dir.display());
            }
            if r.is_error {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Runs { limit } => {
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(jack_home().join("runs"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("result.json").exists())
                .collect();
            dirs.sort();
            dirs.reverse();
            println!("{:<22} {:<7} {:<6} {:>6} {:>8} {:>8} {:>8}  task", "run", "engine", "status", "tools", "cache rd", "output", "sess $");
            for d in dirs.into_iter().take(limit) {
                let r: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("result.json"))?)?;
                let spec: serde_json::Value = std::fs::read_to_string(d.join("spec.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
                let task: String = spec["prompt"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(50).collect();
                println!(
                    "{:<22} {:<7} {:<6} {:>6} {:>8} {:>8} {:>8}  {task}",
                    d.file_name().unwrap_or_default().to_string_lossy(),
                    r["engine"].as_str().unwrap_or("?"),
                    if r["is_error"].as_bool().unwrap_or(false) { "error" } else { "ok" },
                    r["tool_calls"].as_array().map_or(0, Vec::len),
                    human(r["usage"]["cache_read"].as_i64().unwrap_or(0)),
                    human(r["usage"]["output"].as_i64().unwrap_or(0)),
                    r["session_cost_usd"].as_f64().map_or("-".into(), |c| format!("{c:.3}")),
                );
            }
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
