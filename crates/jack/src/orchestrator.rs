//! Orchestrator (design doc, Problem 5): route a task to an engine, isolate
//! it in its own git worktree and port range, brief it from jack's store,
//! run it, and keep its changes on a branch until you merge or drop them.
//!
//! State is plain files written by one process each: `tasks/<id>/task.json`
//! (owned by the running task, later by merge/drop), `ports/<slot>` (claimed
//! with create-new, so two tasks can never get the same range).

use anyhow::{Context, Result, bail};
use jack_engines::{Access, Engine, Event, RunResult, RunSpec};
use jack_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PORT_BASE: u16 = 41000;
const PORTS_PER_TASK: u16 = 20;
const PORT_SLOTS: u16 = 200;
const BRIEFING_MAX_BYTES: usize = 6000;

// ------------------------------------------------------------ routing

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub engine: Engine,
    pub access: Access,
    pub reason: String,
}

/// Rules-first routing: cheap, predictable, and printed so you can see why.
/// `claude_quota_5h` is the last seen utilization of Claude's 5-hour window.
pub fn route(task: &str, claude_quota_5h: Option<f64>) -> Route {
    let lower = task.to_lowercase();
    // Keep identifiers whole (`--dry-run`, `test_utils`), so a word inside a
    // name doesn't count as an instruction.
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .map(|w| w.trim_matches('-'))
        .filter(|w| !w.is_empty())
        .collect();
    let has = |ws: &[&str]| ws.iter().any(|w| words.contains(w));

    let asks = words.first().is_some_and(|w| {
        ["what", "why", "how", "where", "which", "who", "explain", "describe", "review", "summarize", "summarise", "find", "list", "show", "is", "are", "does", "do", "can"].contains(w)
    }) || lower.trim_end().ends_with('?');
    let changes = has(&["fix", "change", "add", "implement", "update", "refactor", "rename", "remove", "delete", "write", "create", "migrate", "bump"]);
    let runs = has(&["test", "tests", "build", "run", "compile", "lint", "install", "bench", "benchmark", "ci"]);
    let (access, access_why) = if asks && !changes {
        (Access::Read, "a question, no change asked")
    } else if runs {
        (Access::Full, "needs to run commands")
    } else {
        (Access::Edit, "edits only")
    };

    let heavy = has(&["debug", "design", "architecture", "refactor", "investigate", "why", "race", "deadlock", "performance", "security", "plan", "flaky"])
        || task.len() > 400;
    let light = task.len() < 160
        && has(&["rename", "typo", "format", "comment", "comments", "docstring", "flag", "bump", "small", "simple", "trivial", "readme"]);
    let (engine, engine_why) = match claude_quota_5h {
        Some(q) if q > 0.9 => (Engine::Cursor, format!("Claude 5h quota at {:.0}%", q * 100.0)),
        _ if heavy => (Engine::Claude, "debugging/design work".to_string()),
        _ if light => (Engine::Cursor, "small, well-defined change".to_string()),
        _ => (Engine::Claude, "default".to_string()),
    };
    Route { engine, access, reason: format!("{} ({engine_why}), {} ({access_why})", engine.name(), access_name(access)) }
}

pub fn access_name(a: Access) -> &'static str {
    match a {
        Access::Read => "read",
        Access::Edit => "edit",
        Access::Full => "full",
    }
}

// ------------------------------------------------------------ git

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).stdin(Stdio::null()).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn repo_root(cwd: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).context("not inside a git repository")?))
}

fn repo_name(repo: &Path) -> String {
    repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into())
}

// ------------------------------------------------------------ tasks

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    /// Commit the worktree started from.
    pub base: String,
    pub port_base: u16,
    pub engine: String,
    pub access: Access,
    pub route_reason: String,
    pub prompt: String,
    /// running, ready (changes to review), done (no changes), failed, merged, dropped
    pub status: String,
    pub session_id: Option<String>,
    pub summary: Option<String>,
    pub files_changed: Vec<String>,
    pub commit: Option<String>,
    pub run_dir: Option<PathBuf>,
    pub created: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn task_dir(home: &Path, id: &str) -> PathBuf {
    home.join("tasks").join(id)
}

/// Write via a temp file + rename, so readers never see half a file.
fn write_atomic(path: &Path, data: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save_task(home: &Path, t: &Task) -> Result<()> {
    let dir = task_dir(home, &t.id);
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join("task.json"), &serde_json::to_string_pretty(t)?)
}

pub fn load_task(home: &Path, id: &str) -> Result<Task> {
    let all = list_tasks(home)?;
    let hits: Vec<Task> = all.into_iter().filter(|t| t.id.starts_with(id)).collect();
    match hits.len() {
        1 => Ok(hits.into_iter().next().expect("one")),
        0 => bail!("no task starts with {id:?}"),
        n => bail!("{n} tasks start with {id:?}; give more of the id"),
    }
}

/// All tasks, newest first.
pub fn list_tasks(home: &Path) -> Result<Vec<Task>> {
    let mut out: Vec<Task> = std::fs::read_dir(home.join("tasks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("task.json")).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    out.sort_by_key(|t| std::cmp::Reverse((t.created, t.id.clone())));
    Ok(out)
}

fn new_id(home: &Path) -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let mut id = format!("{:x}", ms ^ (std::process::id() as u128) << 20);
    id = id[id.len().saturating_sub(7)..].to_string();
    while task_dir(home, &id).exists() {
        id.push('x');
    }
    id
}

// ------------------------------------------------------------ ports

/// Claim a free port range. A slot is a file created with create-new, so
/// two tasks racing for the same slot can't both win.
pub fn claim_ports(home: &Path, id: &str) -> Result<(u16, PathBuf)> {
    let dir = home.join("ports");
    std::fs::create_dir_all(&dir)?;
    for slot in 0..PORT_SLOTS {
        let p = dir.join(slot.to_string());
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            use std::io::Write;
            let _ = write!(f, "{id}");
            return Ok((PORT_BASE + slot * PORTS_PER_TASK, p));
        }
    }
    bail!("all {PORT_SLOTS} port ranges are in use; drop finished tasks or clear {}", dir.display())
}

// ------------------------------------------------------------ briefing

fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

/// What the agent should know before starting, built from jack's store:
/// repo state, earlier tasks here, checkpoint notes, recent Claude sessions.
pub fn briefing(home: &Path, repo: &Path, task: &Task) -> String {
    let mut b = String::from("## Briefing from jack\n\n");
    let name = repo_name(repo);
    let subject = git(repo, &["log", "-1", "--format=%h %s", &task.base]).unwrap_or_default();
    b += &format!(
        "- You are working in an isolated git worktree of `{name}` on branch `{}`, based on {subject}. \
         jack commits your changes to that branch when you finish; you do not need to commit.\n",
        task.branch
    );
    b += &format!(
        "- If you start servers, use ports {}-{} (env JACK_PORT_BASE); other tasks may be running in parallel.\n",
        task.port_base,
        task.port_base + PORTS_PER_TASK - 1
    );
    let dirty = git(repo, &["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0);
    if dirty > 0 {
        b += &format!("- The main checkout has {dirty} uncommitted changes that are NOT in this worktree.\n");
    }

    let earlier: Vec<Task> = list_tasks(home)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.repo == repo && t.id != task.id && t.status != "running")
        .take(5)
        .collect();
    if !earlier.is_empty() {
        b += "\n### Earlier jack tasks in this repo (newest first)\n";
        for t in earlier {
            b += &format!("- [{}] {} -> {}", t.status, clip(&t.prompt, 80), clip(t.summary.as_deref().unwrap_or(""), 160));
            if !t.files_changed.is_empty() {
                b += &format!(" (files: {})", t.files_changed.join(", "));
            }
            b += "\n";
        }
    }

    // Checkpoint notes from jack MCP sessions rooted in this repo or its task worktrees.
    let wt_prefix = home.join("worktrees").join(format!("{name}-"));
    let mut notes: Vec<(i64, String)> = Vec::new();
    let mut claude_sessions: Vec<(std::time::SystemTime, String)> = Vec::new();
    for e in std::fs::read_dir(home.join("sessions")).into_iter().flatten().flatten() {
        let db = e.path().join("store.db");
        let Ok(store) = Store::open_readonly(&db) else { continue };
        let fname = e.file_name().to_string_lossy().into_owned();
        if fname.starts_with("cc-") {
            if store.get_meta("cwd").ok().flatten().is_some_and(|c| Path::new(&c) == repo)
                && let (Ok(Some(first)), Ok(m)) = (store.first_text("user"), db.metadata())
            {
                claude_sessions.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), first));
            }
            continue;
        }
        let rooted = store.events_of_kind("session_start").unwrap_or_default().iter().any(|(_, p)| {
            p["root"].as_str().is_some_and(|r| Path::new(r) == repo || r.starts_with(&*wt_prefix.to_string_lossy()))
        });
        if rooted {
            for (ts, p) in store.events_of_kind("checkpoint").unwrap_or_default() {
                if let Some(n) = p["note"].as_str() {
                    notes.push((ts, n.to_string()));
                }
            }
        }
    }
    if !notes.is_empty() {
        notes.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
        b += "\n### Checkpoint notes (newest first)\n";
        for (_, n) in notes.iter().take(10) {
            b += &format!("- {}\n", clip(n, 200));
        }
    }
    if !claude_sessions.is_empty() {
        claude_sessions.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        b += "\n### Recent Claude Code sessions in this repo (their first request)\n";
        for (_, first) in claude_sessions.iter().take(3) {
            b += &format!("- {}\n", clip(first, 120));
        }
    }
    if b.len() > BRIEFING_MAX_BYTES {
        let mut cut = BRIEFING_MAX_BYTES;
        while !b.is_char_boundary(cut) {
            cut -= 1;
        }
        b.truncate(cut);
        b += "\n[briefing cut]\n";
    }
    b
}

// ------------------------------------------------------------ running

pub struct TaskOpts {
    pub engine: Option<Engine>,
    pub access: Option<Access>,
    pub model: Option<String>,
    pub jack_tools: bool,
    pub timeout: Duration,
}

fn last_quota(home: &Path) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("quota.json")).ok()?).ok()?;
    // Ignore readings older than the 5-hour window itself.
    if now().saturating_sub(v["at"].as_u64()?) > 5 * 3600 {
        return None;
    }
    v["claude_5h"].as_f64()
}

fn remove_worktree(t: &Task) {
    let _ = git(&t.repo, &["worktree", "remove", "--force", &t.worktree.to_string_lossy()]);
    let _ = git(&t.repo, &["branch", "-D", &t.branch]);
}

/// Route, isolate, brief, run, and commit a task. Returns the final task record.
pub fn run_task(home: &Path, cwd: &Path, prompt: &str, opts: TaskOpts, on_event: impl FnMut(&Event)) -> Result<Task> {
    let repo = repo_root(cwd)?;
    let mut r = route(prompt, last_quota(home));
    if let Some(e) = opts.engine {
        r.engine = e;
        r.reason = format!("{} (chosen), {}", e.name(), r.reason.split_once(", ").map_or("", |x| x.1));
    }
    if let Some(a) = opts.access {
        r.access = a;
        r.reason = format!("{}, {} (chosen)", r.reason.split_once(", ").map_or(r.reason.as_str(), |x| x.0), access_name(a));
    }
    jack_engines::check_installed(r.engine)?;

    let id = new_id(home);
    let (port_base, slot) = claim_ports(home, &id)?;
    let release = || {
        let _ = std::fs::remove_file(&slot);
    };
    let base = match git(&repo, &["rev-parse", "HEAD"]) {
        Ok(b) => b,
        Err(e) => {
            release();
            return Err(e.context("the repository needs at least one commit"));
        }
    };
    let branch = format!("jack/{id}");
    let worktree = home.join("worktrees").join(format!("{}-{id}", repo_name(&repo)));
    if let Err(e) = git(&repo, &["worktree", "add", "-q", "-b", &branch, &worktree.to_string_lossy(), &base]) {
        release();
        return Err(e);
    }
    let mut task = Task {
        id: id.clone(),
        repo: repo.clone(),
        worktree: worktree.clone(),
        branch,
        base,
        port_base,
        engine: r.engine.name().to_string(),
        access: r.access,
        route_reason: r.reason.clone(),
        prompt: prompt.to_string(),
        status: "running".into(),
        session_id: None,
        summary: None,
        files_changed: vec![],
        commit: None,
        run_dir: None,
        created: now(),
    };
    save_task(home, &task)?;

    let result = (|| -> Result<RunResult> {
        let exe = std::env::current_exe()?;
        let mut jack_tools = None;
        if opts.jack_tools {
            if r.engine == Engine::Cursor {
                // Cursor reads MCP servers from .cursor/mcp.json and project
                // permissions from .cursor/cli.json. Headless runs can't
                // approve tools, so pre-allow the jack tools this access
                // level permits, in Cursor's `Mcp(server:tool)` form.
                let cfg = worktree.join(".cursor/mcp.json");
                let perms = worktree.join(".cursor/cli.json");
                for f in [&cfg, &perms] {
                    if f.exists() {
                        bail!("{} already exists; not overwriting it to attach jack", f.display());
                    }
                }
                std::fs::create_dir_all(cfg.parent().expect("has parent"))?;
                let mcp = json!({"mcpServers": {"jack": {"command": exe, "args": ["mcp", "--root", worktree]}}});
                std::fs::write(&cfg, serde_json::to_string_pretty(&mcp)?)?;
                let allow: Vec<String> = jack_engines::jack_tools_allowed(r.access)
                    .iter()
                    .map(|t| format!("Mcp(jack:{})", t.trim_start_matches("mcp__jack__")))
                    .collect();
                std::fs::write(&perms, serde_json::to_string_pretty(&json!({"permissions": {"allow": allow, "deny": []}}))?)?;
            }
            jack_tools = Some(exe);
        }
        let full_prompt = format!("{}\n## Task\n\n{prompt}\n", briefing(home, &repo, &task));
        let spec = RunSpec {
            prompt: full_prompt.clone(),
            cwd: worktree.clone(),
            resume: None,
            model: opts.model.clone(),
            access: r.access,
            jack_tools,
            timeout: opts.timeout,
            env: vec![
                ("JACK_TASK_ID".into(), id.clone()),
                ("JACK_PORT_BASE".into(), port_base.to_string()),
                ("PORT".into(), port_base.to_string()),
            ],
        };
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
        let run_dir = home.join("runs").join(format!("{stamp}-{}", r.engine.name()));
        std::fs::create_dir_all(&run_dir)?;
        std::fs::write(
            run_dir.join("spec.json"),
            json!({"engine": r.engine.name(), "cwd": worktree, "prompt": prompt, "full_prompt": full_prompt, "access": r.access, "task": id}).to_string(),
        )?;
        task.run_dir = Some(run_dir.clone());
        let res = jack_engines::run(r.engine, &spec, &run_dir.join("events.jsonl"), on_event)?;
        std::fs::write(run_dir.join("result.json"), serde_json::to_string_pretty(&res)?)?;
        Ok(res)
    })();
    release();

    match result {
        Err(e) => {
            task.status = "failed".into();
            task.summary = Some(format!("{e:#}"));
            save_task(home, &task)?;
            remove_worktree(&task);
            Err(e)
        }
        Ok(res) => {
            if let Some(q) = res.quota_5h {
                let _ = write_atomic(&home.join("quota.json"), &json!({"claude_5h": q, "at": now()}).to_string());
            }
            task.session_id = res.session_id.clone();
            task.summary = Some(res.summary.clone());
            task.files_changed = res.files_changed.clone();
            if !res.files_changed.is_empty() {
                let mut add = vec!["add", "-A", "--"];
                add.extend(res.files_changed.iter().map(String::as_str));
                let msg = format!("jack: {}\n\nTask {id}, engine {}.", clip(prompt, 60), r.engine.name());
                let committed = git(&worktree, &add).and_then(|_| git(&worktree, &["commit", "-q", "-m", &msg]));
                match committed {
                    Ok(_) => task.commit = git(&worktree, &["rev-parse", "--short", "HEAD"]).ok(),
                    Err(e) => task.summary = Some(format!("{}\n[jack could not commit: {e:#}]", res.summary)),
                }
            }
            task.status = if res.is_error {
                "failed"
            } else if task.commit.is_some() {
                "ready"
            } else {
                "done"
            }
            .into();
            // Nothing to review: don't leave the worktree around.
            if task.commit.is_none() {
                remove_worktree(&task);
            }
            save_task(home, &task)?;
            Ok(task)
        }
    }
}

/// Merge a ready task's branch into the repo's current branch, then clean up.
pub fn merge_task(home: &Path, t: &mut Task) -> Result<String> {
    if t.status != "ready" {
        bail!("task {} is {}, not ready to merge", t.id, t.status);
    }
    let current = git(&t.repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if let Err(e) = git(&t.repo, &["merge", "--no-edit", &t.branch]) {
        let _ = git(&t.repo, &["merge", "--abort"]);
        bail!("merge into {current} failed, nothing changed: {e:#}");
    }
    remove_worktree(t);
    t.status = "merged".into();
    save_task(home, t)?;
    Ok(current)
}

pub fn drop_task(home: &Path, t: &mut Task) -> Result<()> {
    if t.status == "running" {
        bail!("task {} is still running", t.id);
    }
    remove_worktree(t);
    t.status = "dropped".into();
    save_task(home, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_rules() {
        let r = route("Why does the parser panic on empty input?", None);
        assert_eq!((r.engine, r.access), (Engine::Claude, Access::Read), "{}", r.reason);
        let r = route("rename the --dry flag to --dry-run", None);
        assert_eq!((r.engine, r.access), (Engine::Cursor, Access::Edit), "{}", r.reason);
        let r = route("fix the failing tests in auth", None);
        assert_eq!((r.engine, r.access), (Engine::Claude, Access::Full), "{}", r.reason);
        let r = route("debug the flaky upload test", Some(0.95));
        assert_eq!(r.engine, Engine::Cursor, "quota overrides: {}", r.reason);
        assert!(r.reason.contains("95%"));
        let r = route("add a helper to test_utils.rs", None);
        assert_eq!(r.access, Access::Edit, "`test` inside an identifier is not a request to run tests: {}", r.reason);
        let r = route("explain how sessions are stored, then fix the typo in README", None);
        assert_eq!(r.access, Access::Edit, "a change was asked: {}", r.reason);
    }

    #[test]
    fn port_claims_are_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let (a, pa) = claim_ports(dir.path(), "a").unwrap();
        let (b, _) = claim_ports(dir.path(), "b").unwrap();
        assert_eq!((a, b), (PORT_BASE, PORT_BASE + PORTS_PER_TASK));
        std::fs::remove_file(pa).unwrap();
        let (c, _) = claim_ports(dir.path(), "c").unwrap();
        assert_eq!(c, PORT_BASE, "a released slot is reused");
    }

    #[test]
    fn task_records_roundtrip_and_prefix_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let t = Task {
            id: "abc1234".into(),
            repo: "/r".into(),
            worktree: "/w".into(),
            branch: "jack/abc1234".into(),
            base: "deadbeef".into(),
            port_base: PORT_BASE,
            engine: "claude".into(),
            access: Access::Edit,
            route_reason: "x".into(),
            prompt: "p".into(),
            status: "ready".into(),
            session_id: None,
            summary: None,
            files_changed: vec!["a.rs".into()],
            commit: Some("1234567".into()),
            run_dir: None,
            created: 1,
        };
        save_task(dir.path(), &t).unwrap();
        let back = load_task(dir.path(), "abc").unwrap();
        assert_eq!((back.id.as_str(), back.access, back.files_changed.len()), ("abc1234", Access::Edit, 1));
        assert!(load_task(dir.path(), "zzz").is_err());
    }
}
