# jack

A self-hosted, token-efficient coding-agent harness. Design: see the jack design docs
(Problems 1–6). This repo currently covers build steps 1–2: the session store, a recorder for Claude
Code sessions, and the jack MCP server, which any MCP client (Claude Code, Cursor
Agent, Goose) can use today.

## Crates

| Crate | What it does |
| --- | --- |
| `jack-store` | One SQLite file per session: content-addressed blobs, an FTS5 index for `recall`, what each file looked like when last shown, and a tool-call log with raw vs. sent bytes |
| `jack-lsp` | A dependency-free LSP client: starts language servers on demand and answers definition, references, rename and diagnostics |
| `jack-engines` | Engine adapters: run Claude Code or Cursor Agent headless and normalize the result (session id, summary, files changed, tokens, cost) |
| `jack-read` | tree-sitter outlines (Rust, Python, TypeScript/TSX, JavaScript, Go), symbol lookup, line slicing, diffs, output trimming |
| `jack` | The binary: MCP server (`jack mcp`), recorder (`jack hook record`, `jack import`) and CLI (`jack read`, `jack outline`, `jack stats`, `jack sessions`) |

## MCP tools

| Tool | Returns |
| --- | --- |
| `jack_read(path, symbol?, lines?, full?)` | Small files whole; large files as an outline. A re-read returns "unchanged since read #N", or only the diff if the file changed |
| `jack_search(query, path?, fixed?)` | ripgrep hits grouped by file, at most 5 per file |
| `jack_sh(cmd, timeout_secs?)` | Exit code plus output trimmed to head, error/warning lines and tail; the full output is stored |
| `recall(handle?, lines?, query?)` | A page of a stored output, or a keyword search over everything stored this session |
| `checkpoint(note)` | Saves a progress note for later briefings |
| `jack_def(symbol, path, line?)` | The definition's code, via the language server |
| `jack_refs(symbol, path, line?)` | Every reference, grouped by file with each line |
| `jack_rename(symbol, path, new_name, dry_run?)` | Renames across files; all-or-nothing |
| `jack_diagnostics(path)` | Errors and warnings from the language server |

Language servers (Arch): `sudo pacman -S rust-analyzer pyright gopls typescript-language-server`.

## Recording Claude Code sessions

Add `jack hook record` as a `Stop`, `PreCompact` and `SessionEnd` hook in
`.claude/settings.json`. Each Claude session is copied into
`~/.jack/sessions/cc-<session id>/store.db`, including subagent transcripts. Ingest
is incremental and idempotent, and the hook always exits 0 (errors go to
`~/.jack/hook.log`).

```sh
jack import          # backfill every transcript in ~/.claude/projects
jack sessions        # tokens and approximate API cost per session
jack session <id>    # model usage and which tools filled the context
```

## Tasks: routing, worktrees, review

```sh
jack task "fix the failing test in parser.rs"   # routed; runs in its own git worktree
jack task --dry-run "..."                       # show the routing decision and briefing only
jack tasks                                      # status of all tasks
jack review <id>                                # the task branch's diff
jack merge <id>                                 # merge into your current branch, clean up
jack drop <id>                                  # discard the worktree and branch
```

- **Routing** is rule-based and printed: questions get `read` access, anything that
  runs tests or builds gets `full`, and everything else gets `edit`. Debugging and design
  work goes to Claude, small well-defined changes go to Cursor, and Claude is the
  default. When Claude's 5-hour quota passes 90%, tasks go to Cursor. Override with
  `--engine` / `--access`.
- **Isolation**: each task runs in `~/.jack/worktrees/<repo>-<id>` on branch `jack/<id>`,
  with its own 20-port range (`JACK_PORT_BASE`, `PORT`), so parallel tasks don't
  collide. Your working tree is untouched until `jack merge`.
- **Briefing**: the agent gets the repo state, earlier jack tasks in this repo,
  `checkpoint` notes and recent Claude Code sessions, capped at about 1.5k tokens.
- **Results**: changes are committed to the task branch. Tasks with no changes clean
  up after themselves.
- `--jack-tools` works for both engines. For Cursor, jack writes `.cursor/mcp.json` and
  `.cursor/cli.json` (allowlist) into the worktree; they are never committed.

## Driving other agents

```sh
jack run "fix the failing test in parser.rs"                  # Claude Code, edits allowed
jack run --engine cursor "add a --verbose flag"               # Cursor Agent (your Cursor model)
jack run --access read "explain how auth works"               # read-only
jack run --access full "run the tests and fix failures"       # edits + shell
jack run --resume <session id> "now add a test for it"        # continue a session
jack run --jack-tools "..."                                   # Claude with jack's MCP tools; built-in Read disabled
jack runs                                                     # history
```

Each run is saved in `~/.jack/runs/<id>/` (spec, raw event stream, result). Files
changed are taken from git, and files that were already modified before the run are
not counted. With `--jack-tools`, only the jack tools allowed at that access level
are pre-approved: `jack_sh` needs `full`, and `jack_rename` needs `edit`.

## Use

```sh
cargo build --release
# Claude Code: register jack and send file reads through it
claude mcp add jack -- /path/to/jack/target/release/jack mcp
claude --disallowedTools Read
# See how much it saved
jack stats
```

Session stores live in `~/.jack/sessions/<id>/store.db` (override with `JACK_HOME`).
Each MCP server process is one session and the only writer of its file.
