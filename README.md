# jack

A self-hosted, token-efficient coding-agent harness. Design: see the jack design docs
(Problems 1–6). This repo currently covers build steps 1–2: the session store and the
jack MCP server, which any MCP client (Claude Code, Cursor Agent, Goose) can use today.

## Crates

| Crate | What it does |
| --- | --- |
| `jack-store` | One SQLite file per session: content-addressed blobs, an FTS5 index for `recall`, what each file looked like when last shown, and a tool-call log with raw vs. sent bytes |
| `jack-read` | tree-sitter outlines (Rust, Python, TypeScript/TSX, JavaScript, Go), symbol lookup, line slicing, diffs, output trimming |
| `jack` | The binary: MCP server (`jack mcp`) and CLI (`jack read`, `jack outline`, `jack stats`) |

## MCP tools

| Tool | Returns |
| --- | --- |
| `jack_read(path, symbol?, lines?, full?)` | Small files whole; large files as an outline. A re-read returns "unchanged since read #N", or only the diff if the file changed |
| `jack_search(query, path?, fixed?)` | ripgrep hits grouped by file, at most 5 per file |
| `jack_sh(cmd, timeout_secs?)` | Exit code plus output trimmed to head, error/warning lines and tail; the full output is stored |
| `recall(handle?, lines?, query?)` | A page of a stored output, or a keyword search over everything stored this session |
| `checkpoint(note)` | Saves a progress note for later briefings |

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
