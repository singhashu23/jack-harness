# jack

**A self-hosted, token-efficient harness and orchestrator for AI coding agents, written in Rust.**

jack works alongside your coding agents (Claude Code, Cursor Agent) and cuts the tokens they spend
reading code. It gives every agent the same small set of reading, search and shell tools, which
return outlines, diffs and trimmed output instead of whole files and full logs. It records your
sessions so you can see where tokens go. It can also run agents headless on tasks, each one in its
own git worktree with a short briefing, so you review the changes before merging them.

> [!IMPORTANT]
> **This project is AI-generated.** The code, tests and documentation in this repository were
> written by an AI model (Claude, by Anthropic, working through Claude Code), directed and reviewed
> by the repository owner. See [About this project](#about-this-project-ai-generated) for what that
> means and how it was tested.

> [!WARNING]
> **Status: early and experimental.** Build steps 1–3 of the plan are done; the native agent loop
> and the evaluation are not (see [Roadmap](#roadmap)). Interfaces may change.

---

## Contents

- [Why](#why)
- [What it does](#what-it-does)
- [Architecture](#architecture)
- [Install](#install)
- [Quick start](#quick-start)
- [MCP tools](#mcp-tools)
- [CLI reference](#cli-reference)
- [Tasks: routing, worktrees, review](#tasks-routing-worktrees-review)
- [Recording sessions](#recording-sessions)
- [Data layout](#data-layout)
- [Design decisions](#design-decisions)
- [Testing](#testing)
- [Limitations](#limitations)
- [Roadmap](#roadmap)
- [Prior art](#prior-art)
- [About this project (AI-generated)](#about-this-project-ai-generated)
- [License](#license)

## Why

Most of what a coding agent pays for is context it has already seen. Every turn resends the whole
conversation, so a file read early in a long session is paid for again on every later turn. With
prompt caching those re-reads are cheap but not free, and they add up. In one recorded Claude Code
session (13 prompts, 130 tool calls), about 25M input tokens were cache reads: roughly half of that
session's approximate API cost.

The cheapest token is the one that never enters the context. jack attacks that at the point where
tool output is produced:

- A large file comes back as an outline (symbols and line ranges), not its full text.
- Re-reading a file you already saw returns `unchanged since read #3`, or only the diff.
- A long test log comes back as its head, its error lines and its tail. The rest is stored and can
  be paged back with `recall`.

On a 4,153-line Rust file, four reads through jack (outline, one symbol, two re-reads) sent 96% fewer
bytes than the same reads as plain whole-file reads.

## What it does

| Area | What you get |
| --- | --- |
| **Token-efficient tools** (MCP) | `jack_read`, `jack_search`, `jack_sh`, `recall`, `checkpoint`: session-aware reads, grouped search, trimmed command output |
| **Semantic tools** (LSP) | `jack_def`, `jack_refs`, `jack_rename`, `jack_diagnostics`, backed by rust-analyzer, pyright, gopls and typescript-language-server |
| **Session recorder** | Claude Code hooks copy every session into a local store; `jack sessions` shows tokens, approximate API cost and which tools filled the context |
| **Engine adapters** | `jack run` drives Claude Code or Cursor Agent headless and normalizes the result: summary, files changed, tokens, cost |
| **Orchestrator** | `jack task` routes a task to an engine, runs it in its own git worktree with a briefing, and keeps the changes on a branch for `jack review` / `merge` / `drop` |

Everything runs locally. jack doesn't call a model API itself (yet). It talks to the agent CLIs you
already have, using their own logins.

## Architecture

```
                You ──► jack CLI
                          │
┌─────────────────────────▼──────────────────────────────────┐
│ orchestrator (jack task)                                    │
│  rule-based router · git worktree + port range per task     │
│  briefing from the store · review / merge / drop            │
└───┬────────────────────┬─────────────────────┬─────────────┘
    │                    │                     │
┌───▼─────────┐   ┌──────▼───────┐   ┌─────────▼──────┐
│ Claude Code │   │ Cursor Agent │   │ native loop    │  (planned)
│ (claude -p) │   │ (-p)         │   │                │
└───┬─────────┘   └──────┬───────┘   └─────────┬──────┘
    │  MCP (stdio)       │                     │
┌───▼────────────────────▼─────────────────────▼─────────────┐
│ jack MCP server: jack_read · jack_search · jack_sh · recall │
│ checkpoint · jack_def · jack_refs · jack_rename · diagnostics│
└───┬───────────────────────────────┬────────────────────────┘
    │ tree-sitter (in-process)      │ LSP (language servers, started on demand)
┌───▼───────────────────────────────▼────────────────────────┐
│ jack store: one SQLite file per session (FTS5 search,       │
│ content-addressed blobs, file hashes, usage, tool log)      │
└────────────────────────────────────────────────────────────┘
```

### Crates

| Crate | What it does |
| --- | --- |
| [`jack-store`](crates/jack-store) | One SQLite file per session: content-addressed blobs, an FTS5 index for `recall`, the last view of each file, transcript blocks, token usage, and a tool-call log with raw vs. sent bytes. Includes a true read-only open for reading other sessions' stores. |
| [`jack-read`](crates/jack-read) | tree-sitter outlines for Rust, Python, TypeScript/TSX, JavaScript and Go; symbol lookup (`Store::open`, `Class.method`); line slicing; unified diffs; output trimming that keeps error lines |
| [`jack-lsp`](crates/jack-lsp) | A small LSP client with no LSP library dependency: framing, request matching, progress tracking, UTF-8/UTF-16 column handling, and all-or-nothing application of rename edits |
| [`jack-engines`](crates/jack-engines) | Headless adapters for Claude Code and Cursor Agent: command building per access level, stream-json parsing, git-based change detection |
| [`jack`](crates/jack) | The binary: MCP server, recorder, orchestrator and CLI |

About 4,700 lines of Rust in total.

## Install

### Requirements

| Needed for | Requirement |
| --- | --- |
| Building | Rust **1.88+** (edition 2024, let-chains) |
| `jack_search` | [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`) on `PATH` |
| Tasks, change detection | `git` |
| Semantic tools (optional) | Language servers: `rust-analyzer`, `pyright`, `gopls`, `typescript-language-server` |
| Driving agents (optional) | `claude` (Claude Code) and/or `cursor-agent` (Cursor CLI), already logged in |

Tested on Arch Linux. Other Linux distributions should work. macOS and Windows haven't been tried,
and the CLI is Unix-only for now (it resets SIGPIPE through `libc`).

### Build

```sh
git clone https://github.com/singhashu23/jack-harness
cd jack-harness
cargo install --path crates/jack --root ~/.local   # puts `jack` in ~/.local/bin
```

Language servers on Arch:

```sh
sudo pacman -S --needed rust-analyzer pyright gopls typescript-language-server
```

## Quick start

### 1. Give Claude Code jack's tools

```sh
claude mcp add jack -- jack mcp
claude --disallowedTools Read      # make jack_read the only way to read files
```

Each MCP server process is one jack session with its own store. Afterwards, see what it saved:

```sh
jack stats
```

### 2. Record your Claude Code sessions

Add the hook to a project's `.claude/settings.json` (or your user settings):

```json
{
  "hooks": {
    "Stop":       [{ "hooks": [{ "type": "command", "command": "jack hook record" }] }],
    "PreCompact": [{ "hooks": [{ "type": "command", "command": "jack hook record" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "jack hook record" }] }]
  }
}
```

Then backfill what you already have and look at it:

```sh
jack import            # every transcript in ~/.claude/projects
jack sessions          # tokens and approximate API cost per session
jack session <id>      # model usage, and which tools' results filled the context
```

### 3. Hand a task to an agent

```sh
cd your-repo
jack task "fix the failing test in parser.rs"
jack review <id>       # read the diff
jack merge <id>        # or: jack drop <id>
```

## MCP tools

All tools return text shaped to a token budget (about 2K tokens per result by default). Anything
trimmed is stored and can be fetched with `recall`.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `jack_read` | `path`, `symbol?`, `lines?`, `full?` | Small files whole, with line numbers. Large files as an outline. A re-read returns `unchanged since read #N` or a unified diff. `symbol` returns only that function or class. |
| `jack_search` | `query`, `path?`, `fixed?`, `max_files?` | ripgrep hits grouped by file, at most 5 lines per file |
| `jack_sh` | `cmd`, `timeout_secs?` | Exit code plus output trimmed to head, error and warning lines, and tail; long lines are cut |
| `recall` | `handle?`, `lines?`, `query?` | A page of a stored output, or a keyword (BM25) search over everything stored this session |
| `checkpoint` | `note` | Saves a progress note; later task briefings include it |
| `jack_def` | `symbol`, `path`, `line?` | The definition's code, found by the language server (follows imports) |
| `jack_refs` | `symbol`, `path`, `line?`, `max?` | Every reference, grouped by file with the line's code |
| `jack_rename` | `symbol`, `path`, `new_name`, `line?`, `dry_run?` | Renames across files. All new contents are computed before any file is written, so a failure changes nothing. |
| `jack_diagnostics` | `path` | Errors and warnings from the language server (for Rust, including `cargo check`) |

Tools carry MCP annotations (`readOnlyHint`, `destructiveHint`), so clients can run the read-only
ones in plan mode. For the semantic tools, the model names a symbol and a file, and jack finds the
exact position from the tree-sitter outline (or a whole-word match on a given line).

## CLI reference

| Command | What it does |
| --- | --- |
| `jack mcp [--root DIR] [--session ID]` | Run the MCP server on stdio |
| `jack read PATH [--symbol S] [--lines A-B] [--full]` | Read a file the way `jack_read` does |
| `jack outline PATH` | Print a file's symbol outline |
| `jack stats [--session ID]` | Raw vs. sent bytes per tool, across jack sessions |
| `jack hook record` | Claude Code hook entry point (reads the hook payload on stdin; always exits 0) |
| `jack import [PATH...]` | Import Claude Code transcripts (default: all) |
| `jack sessions [--limit N]` / `jack session ID` | Recorded sessions: tokens, approximate cost, top tools |
| `jack run PROMPT [--engine claude\|cursor] [--access read\|edit\|full] [--resume ID] [--model M] [--jack-tools] [--json]` | Run one agent headless in the current directory |
| `jack runs` | Past runs |
| `jack task PROMPT [--engine] [--access] [--model] [--jack-tools] [--dry-run]` | Route, isolate, brief and run a task |
| `jack tasks` / `review ID [--stat]` / `merge ID` / `drop ID` | Manage tasks |

### Access levels

| Level | Claude Code | Cursor Agent | jack tools pre-approved |
| --- | --- | --- | --- |
| `read` | `--permission-mode plan` | `--mode plan` | read-only tools |
| `edit` | `--permission-mode acceptEdits` | default (no shell) | adds `jack_rename` |
| `full` | `--permission-mode bypassPermissions` | `--force` | adds `jack_sh` |

Use `full` only in worktrees or repositories where you are comfortable letting an agent run
commands.

## Tasks: routing, worktrees, review

`jack task` does five things:

1. **Routes** the task with plain, printed rules. Questions get `read` access, tasks that run
   tests or builds get `full`, and everything else gets `edit`. Debugging and design work goes to
   Claude, small well-defined changes (renames, typos, flags) go to Cursor, and Claude is the
   default. When Claude's 5-hour quota is above 90% (as reported in Claude's own stream), work goes
   to Cursor. `--engine` and `--access` override the rules; `--dry-run` shows the decision and the
   briefing without running anything.
2. **Isolates** the work in `~/.jack/worktrees/<repo>-<id>` on branch `jack/<id>`, with its own
   20-port range (`JACK_PORT_BASE`, `PORT`), so parallel tasks don't collide. Port ranges are
   claimed by creating files that must not already exist, so two tasks can never get the same one.
3. **Briefs** the agent with the repo state, earlier jack tasks in the same repo and their results,
   `checkpoint` notes, and the first requests of recent Claude Code sessions there. The briefing is
   capped at about 1.5K tokens.
4. **Runs** the chosen engine headless. With `--jack-tools`, jack's MCP server is attached. Claude
   gets it through `--mcp-config`, with the built-in `Read` disabled. Cursor gets it through
   `.cursor/mcp.json` and a `.cursor/cli.json` allowlist (`Mcp(jack:tool)`), both written into the
   worktree and never committed.
5. **Commits** the changes to the task branch. Nothing touches your working tree until
   `jack merge`, which merges into your current branch and aborts cleanly on conflicts. Tasks that
   changed nothing remove their worktree automatically.

Changed files are found with git. Files that were already modified before the run aren't counted.

## Recording sessions

`jack hook record` copies a Claude Code session, including its subagent transcripts, into
`~/.jack/sessions/cc-<session id>/store.db`.

- **Incremental**: it keeps a byte offset per transcript file and reads only complete lines.
- **Idempotent**: rows are keyed by entry UUID and block index, so running it twice adds nothing.
- **Never in the way**: it takes about 10 ms, always exits 0, and logs errors to `~/.jack/hook.log`.
- **Correct usage totals**: Claude Code splits one API response over several transcript entries that
  all repeat its usage. jack keys usage on the response ID, so tokens are counted once. On a
  finished session the totals matched an independent count exactly.

`jack sessions` reports an approximate pay-as-you-go API price from a built-in price table (cache
writes at the 5-minute rate). On a subscription it measures size, not your bill.

## Data layout

```
~/.jack/                          (override with JACK_HOME)
├── sessions/
│   ├── <time>-<pid>/store.db     one per MCP server process
│   └── cc-<session id>/store.db  one per recorded Claude Code session
├── runs/<time>-<engine>/         spec.json, events.jsonl (raw stream), result.json
├── tasks/<id>/task.json          task records
├── worktrees/<repo>-<id>/        task worktrees (removed on merge/drop)
├── ports/<slot>                  claimed port ranges
├── quota.json                    last seen Claude 5-hour quota utilization
└── hook.log                      recorder errors
```

Every file has one writer. Other sessions' stores are only ever opened read-only.

## Design decisions

- **Keep big things out of context instead of removing them later.** Prompt caching works on
  prefixes: rewriting an early turn makes everything after it a cache miss, and some models reject
  edited history. On Claude Opus 5.5 pricing, cutting R tokens from a P-token context pays off only
  after about 24 × (P − R) / R further turns. So jack shapes output when it is produced.
- **Session-aware reading.** jack remembers what each file looked like when the model last saw it
  (by content hash), so a re-read costs one line or a diff.
- **tree-sitter for speed, LSP for meaning.** Outlines and symbol bodies come from in-process
  tree-sitter parsing with no server. Definitions, references, renames and diagnostics come from
  real language servers, started on first use and kept for the session.
- **One writer per file.** A shared vector database corrupting under concurrent sessions was part
  of the original motivation. Every jack session, run and task writes only its own files. Port claims
  use create-new files, and task records are written atomically (temp file, then rename).
- **Rules before models.** Routing is a few transparent rules, not an LLM call per task.
- **No LSP or MCP libraries.** Both protocols are small enough to implement directly, which keeps
  dependencies down and the behaviour easy to follow.

## Testing

```sh
cargo test                 # 37 tests
cargo clippy --all-targets # no warnings
```

- **Unit tests** cover the store, outlines for each language, diffs and trimming, LSP framing, URIs,
  UTF-16 columns and edit application, transcript ingest (incremental, idempotent, split-response
  usage), routing rules, port claims and git change detection.
- **Parser tests** run against real recorded `stream-json` output from Claude Code and Cursor Agent.
- **Live language-server tests** exercise definition and references against rust-analyzer, pyright,
  gopls and typescript-language-server, plus a two-file rename and diagnostics on Rust. They skip
  themselves when a server isn't installed.
- **End-to-end runs** checked the engine adapters and orchestrator against the real `claude` and
  `cursor-agent` CLIs: edit, resume, read-only, review/merge/drop, parallel tasks, and jack tools
  inside both agents. Those use real quota, so they aren't part of `cargo test`.

## Limitations

- **The native agent loop doesn't exist yet.** With Claude Code and Cursor, jack only controls the
  tools it provides, not how those agents manage their own context.
- jack's tools save tokens only when the agent uses them. Disable the built-in file reader (as shown
  above) to make sure it does.
- Outlines support Rust, Python, TypeScript/TSX, JavaScript and Go. Other files are read in pages.
- Cost figures are approximations from a price table, and Cursor reports no cost at all.
- Task commits use your local git identity. `jack merge` doesn't resolve conflicts; it aborts.
- Unix only (tested on Linux).

## Roadmap

- [x] Per-session store and recorder for Claude Code sessions
- [x] Token-efficient MCP tools (`jack_read`, `jack_search`, `jack_sh`, `recall`, `checkpoint`)
- [x] LSP layer (definition, references, rename, diagnostics)
- [x] Headless adapters for Claude Code and Cursor Agent
- [x] Orchestrator: routing, worktrees, briefings, review/merge/drop
- [ ] Native agent loop with full context control: append-only cached context, output shaping,
      segment resets with briefings built from stored facts
- [ ] Evaluation: cost per solved task against plain Claude Code (a SWE-bench Verified subset plus
      real tasks), including an observation-masking baseline
- [ ] Cross-session memory as immutable segments with an atomic pointer swap
- [ ] Comparison of `jack_read` with [Serena](https://github.com/oraios/serena) on the same tasks
- [ ] More languages for outlines

## Prior art

jack borrows ideas from, and should be compared with:

- **The Complexity Trap** (JetBrains Research and TU Munich, arXiv 2508.21433): masking old
  observations halves agent cost without lowering solve rates.
- **CoACT** (arXiv 2607.02911) and **SWE-Pruner** (arXiv 2601.16746): compressing or pruning
  observations while keeping the actions.
- **ACE: Agentic Context Engineering** (arXiv 2510.04618): contexts should grow by small edits,
  not full rewrites.
- **SWE-agent's agent-computer interface**: tools designed for the model improve results.
- **Serena**, **Aider's repo map**, **Claude Context** (Merkle-tree indexing): symbol-level code
  access for agents.
- **rtk**, **Headroom**, **Caveman**: trimming command and tool output.
- **Emdash**, **Agent Orchestrator**, **AWS cli-agent-orchestrator**: running agent CLIs in parallel
  worktrees.

## About this project (AI-generated)

This repository was **written by AI**: by Claude (Anthropic's model, Claude Opus 5.5) working in
Claude Code, over a series of conversations in which the repository owner set the goals, made the
design decisions (for example Rust, a hand-written `jack_read` instead of a fork, and keeping
existing tools alongside jack), and approved each step.

What that means in practice:

- **The design, code, tests, commit messages and this README were generated by the model.** The
  owner directed the work and reviewed the results, but not every line was reviewed by hand.
- **Claims here were checked by running things**, not just written down. Every component has tests,
  and the live tests and end-to-end runs listed under [Testing](#testing) were run against real
  language servers and real agent CLIs. Several bugs were found and fixed that way, for example
  double-counted usage, tsserver answering before its project had loaded, and MCP permission
  handling in both agents. Each commit message describes what was verified in that step.
- **It may still contain mistakes**: untested edge cases, platform assumptions, or behaviour that
  depends on the CLI versions current in late 2026 (Claude Code 2.1.x, Cursor Agent 2026.10).
- Read the code before relying on it, especially before using `--access full`, which lets an agent
  run shell commands.

Issues and pull requests are welcome.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
