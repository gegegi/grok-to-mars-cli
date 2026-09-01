//! Test double for session actors. Not installed by `scripts/install-gtm.sh`.
//! Speaks ACP NDJSON on stdio like `gtm agent --no-leader stdio`.

use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn main() {
    let mut out = io::stdout().lock();
    let mut sid = format!("fake-{}", std::process::id());
    let mut pending_prompt_id: Option<Value> = None;

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if pending_prompt_id.is_some() && msg.get("method").is_none() && msg.get("id").is_some() {
            let prompt_id = pending_prompt_id.take().unwrap();
            finish_prompt(&mut out, prompt_id, &sid);
            continue;
        }

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => reply(
                &mut out,
                id,
                json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "gtm-hub-fake-agent", "version": "0.0.0" },
                    "agentCapabilities": { "loadSession": true, "resumeSession": true },
                }),
            ),
            "session/new" => reply(&mut out, id, json!({ "sessionId": sid })),
            "session/load" | "session/resume" => {
                if let Some(s) = params.get("sessionId").and_then(|v| v.as_str()) {
                    sid = s.to_string();
                }
                reply(&mut out, id, json!({ "sessionId": sid }));
            }
            "session/prompt" => {
                if prompt_text(&params).contains("NEED_PERM") {
                    pending_prompt_id = id;
                    write_msg(
                        &mut out,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": "perm-1",
                            "method": "session/request_permission",
                            "params": {
                                "sessionId": sid,
                                "toolCall": { "toolCallId": "t1", "title": "echo" }
                            }
                        }),
                    );
                } else if let Some(id) = id {
                    finish_prompt(&mut out, id, &sid);
                }
            }
            "session/cancel" => {}
            _ => {
                if let Some(id) = id {
                    reply(&mut out, Some(id), json!({}));
                }
            }
        }
    }
}

fn prompt_text(params: &Value) -> String {
    let mut s = String::new();
    if let Some(arr) = params.get("prompt").and_then(|p| p.as_array()) {
        for block in arr {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                s.push_str(t);
            }
        }
    }
    s
}

fn finish_prompt(out: &mut impl Write, prompt_id: Value, sid: &str) {
    write_msg(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hello from hub" }
                }
            }
        }),
    );
    reply(out, Some(prompt_id), json!({ "stopReason": "end_turn" }));
}

fn reply(out: &mut impl Write, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    write_msg(
        out,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    );
}

fn write_msg(out: &mut impl Write, msg: &Value) {
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}
