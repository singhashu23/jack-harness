//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0).
//! Implements initialize, ping, tools/list and tools/call, which is all any
//! MCP client (Claude Code, Cursor Agent, Goose) needs for tools.

use crate::tools::{tool_list, Tools};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const DEFAULT_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "jack keeps tool output small. Use jack_read instead of other file-reading tools: \
it returns outlines for large files, and 'unchanged' or a diff when you re-read a file. Use jack_search to find \
code and jack_sh for commands with long output. Trimmed output is never lost: page it with recall.";

fn handle(tools: &Tools, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));

    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "jack", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_list() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let out = tools.call(name, &args);
            Ok(json!({
                "content": [{"type": "text", "text": out.text}],
                "isError": out.is_error
            }))
        }
        m if m.starts_with("notifications/") => return None,
        m => Err((-32601, format!("method not found: {m}"))),
    };

    // Notifications (no id) never get a response.
    let id = id?;
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

pub fn serve(tools: Tools) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| handle(&tools, m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => handle(&tools, &msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            })),
        };
        if let Some(r) = reply {
            writeln!(stdout, "{r}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jack_store::Store;

    #[test]
    fn handshake_and_call() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(Store::open_in_memory().unwrap(), dir.path().canonicalize().unwrap());
        let init = handle(&tools, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&tools, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let list = handle(&tools, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 9);
        let call = handle(&tools, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"jack_sh","arguments":{"cmd":"echo hi"}}})).unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("hi"));
        let bad = handle(&tools, &json!({"jsonrpc":"2.0","id":4,"method":"nope"})).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }
}
