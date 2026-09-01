use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
fn grok_sessions_root() -> PathBuf {
    if let Some(home) = std::env::var_os("GROK_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join("sessions");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
        .join("sessions")
}

/// Cold sessions from `~/.grok/sessions/<group>/<id>/summary.json`.
pub fn list_disk_sessions() -> Vec<Value> {
    list_disk_sessions_in(&grok_sessions_root())
}

pub fn list_disk_sessions_in(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let groups = match fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for group in groups.flatten() {
        if !group.path().is_dir() {
            continue;
        }
        let kids = match fs::read_dir(group.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for session in kids.flatten() {
            let dir = session.path();
            let summary = dir.join("summary.json");
            if !summary.is_file() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&summary) else {
                continue;
            };
            let parsed: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            if is_subagent(&parsed) {
                continue;
            }
            let id = session.file_name().to_string_lossy().into_owned();
            let cwd = session_cwd(&parsed, &group.path());
            out.push(json!({
                "id": id,
                "title": session_title(&parsed),
                "live": false,
                "cwd": cwd,
            }));
        }
    }
    out.sort_by(|a, b| {
        let ta = a.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("title").and_then(|v| v.as_str()).unwrap_or("");
        ta.cmp(tb)
    });
    out
}

fn session_title(parsed: &Value) -> String {
    parsed
        .get("generated_title")
        .or_else(|| parsed.get("generatedTitle"))
        .or_else(|| parsed.get("session_summary"))
        .or_else(|| parsed.get("sessionSummary"))
        .or_else(|| parsed.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn session_cwd(parsed: &Value, group: &Path) -> Value {
    if let Some(cwd) = parsed
        .get("cwd")
        .or_else(|| parsed.pointer("/info/cwd"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return json!(cwd);
    }
    let cwd_file = group.join(".cwd");
    if let Ok(text) = fs::read_to_string(&cwd_file) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return json!(trimmed);
        }
    }
    let name = group.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(decoded) = percent_decode(name) {
        if decoded.starts_with('/') {
            return json!(decoded);
        }
    }
    Value::Null
}

fn is_subagent(parsed: &Value) -> bool {
    parsed
        .get("session_kind")
        .or_else(|| parsed.get("sessionKind"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase().starts_with("subagent"))
        .unwrap_or(false)
}

fn percent_decode(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn lookup_title(session_id: &str) -> Option<String> {
    lookup_summary(session_id).and_then(|(parsed, _)| {
        let t = session_title(&parsed);
        (!t.is_empty()).then_some(t)
    })
}

/// Best-effort cwd for a session id from disk (group `.cwd` / `info.cwd`).
pub fn lookup_cwd(session_id: &str) -> Option<String> {
    lookup_summary(session_id).and_then(|(parsed, group)| {
        session_cwd(&parsed, &group)
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    })
}

pub const HISTORY_DEFAULT: usize = 200;
pub const HISTORY_MAX: usize = 500;
const HISTORY_TEXT_MAX: usize = 8_000;
const TOOL_TEXT_MAX: usize = 240;
const DIFF_TEXT_MAX: usize = 4_000;

pub fn lookup_session_dir(session_id: &str) -> Option<PathBuf> {
    let root = grok_sessions_root();
    let groups = fs::read_dir(root).ok()?;
    for group in groups.flatten() {
        if !group.path().is_dir() {
            continue;
        }
        let dir = group.path().join(session_id);
        if dir.join("summary.json").is_file() || dir.join("chat_history.jsonl").is_file() {
            return Some(dir);
        }
    }
    None
}

/// Oldest → newest transcript rows for a session (`chat_history.jsonl`, else `updates.jsonl`).
pub fn session_history(session_id: &str, limit: usize) -> Vec<Value> {
    let Some(dir) = lookup_session_dir(session_id) else {
        return Vec::new();
    };
    session_history_in(&dir, limit)
}

pub fn session_history_in(dir: &Path, limit: usize) -> Vec<Value> {
    let limit = if limit == 0 {
        HISTORY_DEFAULT
    } else {
        limit.min(HISTORY_MAX)
    };
    let hist = dir.join("chat_history.jsonl");
    if hist.is_file() {
        let msgs = parse_chat_history(&hist);
        if !msgs.is_empty() {
            return take_last(msgs, limit);
        }
    }
    let updates = dir.join("updates.jsonl");
    if updates.is_file() {
        return take_last(parse_updates(&updates), limit);
    }
    Vec::new()
}

fn take_last(mut msgs: Vec<Value>, limit: usize) -> Vec<Value> {
    if msgs.len() > limit {
        msgs.drain(0..msgs.len() - limit);
    }
    msgs
}

fn parse_chat_history(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "user" => {
                let body = strip_env_noise(&extract_user_text(obj.get("content")));
                if body.is_empty() {
                    continue;
                }
                out.push(history_row("user", &body, None));
            }
            "assistant" => {
                if let Some(tools) = obj.get("tool_calls").and_then(|v| v.as_array()) {
                    for tool in tools {
                        let name = tool
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool");
                        let args_val = tool.get("arguments");
                        let args_owned = args_val
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                args_val
                                    .map(|v| v.to_string())
                                    .unwrap_or_default()
                            });
                        let preview = truncate(&args_owned, TOOL_TEXT_MAX);
                        let text = if preview.is_empty() {
                            "called"
                        } else {
                            preview.as_str()
                        };
                        let mut row = history_row("tool", text, Some(name));
                        if let Some(val) = args_val {
                            if let Some(s) = val.as_str() {
                                attach_diff(&mut row, tool_args_diff(s).as_ref());
                            } else {
                                attach_diff(&mut row, tool_args_diff_value(val).as_ref());
                            }
                        }
                        out.push(row);
                    }
                }
                let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let content = content.trim();
                if !content.is_empty() {
                    out.push(history_row("assistant", &truncate(content, HISTORY_TEXT_MAX), None));
                }
            }
            "reasoning" => {
                let thought = extract_reasoning(&obj);
                if thought.is_empty() {
                    continue;
                }
                out.push(history_row("thought", &truncate(&thought, HISTORY_TEXT_MAX), None));
            }
            "tool_result" => {
                let body = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    continue;
                }
                out.push(history_row(
                    "tool",
                    &truncate(trimmed, TOOL_TEXT_MAX),
                    Some("result"),
                ));
            }
            _ => {}
        }
    }
    out
}

fn parse_updates(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut role: Option<&'static str> = None;
    let mut buf = String::new();
    let mut tool_title: Option<String> = None;

    let flush = |out: &mut Vec<Value>,
                 role: &mut Option<&'static str>,
                 buf: &mut String,
                 tool_title: &mut Option<String>| {
        let t = buf.trim();
        if let Some(r) = *role {
            if !t.is_empty() {
                let cap = if r == "tool" {
                    TOOL_TEXT_MAX
                } else {
                    HISTORY_TEXT_MAX
                };
                out.push(history_row(r, &truncate(t, cap), tool_title.as_deref()));
            }
        }
        *role = None;
        buf.clear();
        *tool_title = None;
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let method = obj.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method != "session/update" && !method.ends_with("session/update") {
            continue;
        }
        let update = obj
            .pointer("/params/update")
            .cloned()
            .unwrap_or(json!({}));
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let chunk = update
            .pointer("/content/text")
            .or_else(|| update.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match kind {
            "user_message_chunk" => {
                if role != Some("user") {
                    flush(&mut out, &mut role, &mut buf, &mut tool_title);
                    role = Some("user");
                }
                buf.push_str(chunk);
            }
            "agent_thought_chunk" => {
                if role != Some("thought") {
                    flush(&mut out, &mut role, &mut buf, &mut tool_title);
                    role = Some("thought");
                }
                buf.push_str(chunk);
            }
            "agent_message_chunk" | "agent_message" => {
                if role != Some("assistant") {
                    flush(&mut out, &mut role, &mut buf, &mut tool_title);
                    role = Some("assistant");
                }
                buf.push_str(chunk);
            }
            "tool_call" => {
                flush(&mut out, &mut role, &mut buf, &mut tool_title);
                let title = update
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Tool");
                let status = update
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ran");
                let mut row = history_row("tool", status, Some(title));
                attach_diff(&mut row, extract_diff(&update).as_ref());
                out.push(row);
            }
            "tool_call_update" => {
                if let Some(last) = out.last_mut() {
                    if last.get("role").and_then(|v| v.as_str()) == Some("tool") {
                        if let Some(status) = update.get("status").and_then(|v| v.as_str()) {
                            last["text"] = json!(status);
                        }
                        attach_diff(last, extract_diff(&update).as_ref());
                    }
                }
            }
            _ => {}
        }
    }
    flush(&mut out, &mut role, &mut buf, &mut tool_title);
    for row in &mut out {
        if row.get("role").and_then(|v| v.as_str()) == Some("user") {
            if let Some(text) = row.get("text").and_then(|v| v.as_str()) {
                let cleaned = strip_env_noise(text);
                row["text"] = json!(cleaned);
            }
        }
    }
    out.retain(|row| {
        row.get("text")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    out
}

fn history_row(role: &str, text: &str, tool_title: Option<&str>) -> Value {
    let mut row = json!({ "role": role, "text": text });
    if let Some(title) = tool_title {
        row["toolTitle"] = json!(title);
    }
    row
}

fn attach_diff(row: &mut Value, diff: Option<&(String, String, String)>) {
    let Some((path, old, new)) = diff else { return };
    row["diffPath"] = json!(path);
    row["oldText"] = json!(truncate(old, DIFF_TEXT_MAX));
    row["newText"] = json!(truncate(new, DIFF_TEXT_MAX));
}

fn extract_diff(update: &Value) -> Option<(String, String, String)> {
    if let Some(found) = content_diff(update.get("content")) {
        return Some(found);
    }
    let raw = update
        .get("rawInput")
        .or_else(|| update.get("raw_input"))?;
    tool_args_diff_value(raw)
}

fn content_diff(content: Option<&Value>) -> Option<(String, String, String)> {
    let content = content?;
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(found) = content_diff(Some(item)) {
                return Some(found);
            }
        }
        return None;
    }
    let obj = content.as_object()?;
    let kind = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if kind == "diff" {
        let path = obj
            .get("path")
            .or_else(|| obj.get("filePath"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?;
        let old = obj
            .get("oldText")
            .or_else(|| obj.get("old_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = obj
            .get("newText")
            .or_else(|| obj.get("new_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Some((path.to_string(), old.to_string(), new.to_string()));
    }
    content_diff(obj.get("content"))
}

fn tool_args_diff(args: &str) -> Option<(String, String, String)> {
    let parsed: Value = serde_json::from_str(args).ok()?;
    tool_args_diff_value(&parsed)
}

fn tool_args_diff_value(raw: &Value) -> Option<(String, String, String)> {
    let path = raw
        .get("file_path")
        .or_else(|| raw.get("filePath"))
        .or_else(|| raw.get("path"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let old = raw
        .get("old_string")
        .or_else(|| raw.get("oldString"))
        .or_else(|| raw.get("oldText"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new = raw
        .get("new_string")
        .or_else(|| raw.get("newString"))
        .or_else(|| raw.get("newText"))
        .or_else(|| raw.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if old.is_empty() && new.is_empty() {
        return None;
    }
    Some((path.to_string(), old.to_string(), new.to_string()))
}

fn extract_user_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn extract_reasoning(obj: &Value) -> String {
    obj.get("summary")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn strip_env_noise(raw: &str) -> String {
    if let Some(start) = raw.find("<user_query>") {
        if let Some(end) = raw.find("</user_query>") {
            if end > start {
                let inner = &raw[start + "<user_query>".len()..end];
                let inner = inner.trim();
                if !inner.is_empty() {
                    return inner.to_string();
                }
            }
        }
    }
    let trimmed = raw.trim();
    if trimmed.len() > 4_000 {
        let tail: String = trimmed.chars().rev().take(2_000).collect();
        let tail: String = tail.chars().rev().collect();
        return format!("…{tail}");
    }
    trimmed.to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(1);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn lookup_summary(session_id: &str) -> Option<(Value, PathBuf)> {
    let root = grok_sessions_root();
    let groups = fs::read_dir(root).ok()?;
    for group in groups.flatten() {
        if !group.path().is_dir() {
            continue;
        }
        let dir = group.path().join(session_id);
        let summary = dir.join("summary.json");
        if !summary.is_file() {
            continue;
        }
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&summary).ok()?).unwrap_or(json!({}));
        return Some((parsed, group.path()));
    }
    None
}

/// Live actors first, then cold disk cards not already live.
pub fn merge_live(live: Vec<Value>, disk: Vec<Value>) -> Vec<Value> {
    let live_ids: std::collections::HashSet<String> = live
        .iter()
        .filter_map(|v| {
            v.get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    let disk_title: std::collections::HashMap<String, String> = disk
        .iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?.to_string();
            let title = c.get("title")?.as_str()?.to_string();
            (!title.is_empty()).then_some((id, title))
        })
        .collect();
    let mut out: Vec<Value> = live
        .into_iter()
        .map(|mut card| {
            let empty = card
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty();
            if empty {
                if let Some(id) = card.get("id").and_then(|v| v.as_str()) {
                    if let Some(title) = disk_title.get(id) {
                        card["title"] = json!(title);
                    }
                }
            }
            card
        })
        .collect();
    for card in disk {
        let id = card.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !live_ids.contains(id) {
            out.push(card);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_summary_cards() {
        let root = std::env::temp_dir().join(format!("gtm-hub-sess-{}", std::process::id()));
        let dir = root.join("encoded-cwd").join("abc-123");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"Hello","info":{"cwd":"/tmp/proj"}}"#,
        )
        .unwrap();
        let list = list_disk_sessions_in(&root);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "abc-123");
        assert_eq!(list[0]["title"], "Hello");
        assert_eq!(list[0]["live"], false);
        assert_eq!(list[0]["cwd"], "/tmp/proj");

        let nested = root.join("%2FUsers%2Fme%2Fapp").join("sess-2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("summary.json"), r#"{"title":"From group"}"#).unwrap();
        let list = list_disk_sessions_in(&root);
        let card = list.iter().find(|c| c["id"] == "sess-2").unwrap();
        assert_eq!(card["cwd"], "/Users/me/app");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_prefers_live() {
        let live = vec![json!({"id": "abc-123", "title": "Hello", "live": true})];
        let disk = vec![
            json!({"id": "abc-123", "title": "Hello", "live": false}),
            json!({"id": "cold", "title": "Old", "live": false}),
        ];
        let merged = merge_live(live, disk);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["id"], "abc-123");
        assert_eq!(merged[0]["live"], true);
        assert_eq!(merged[1]["id"], "cold");
    }

    #[test]
    fn merge_keeps_disk_title_when_live_title_blank() {
        let live = vec![json!({"id": "abc-123", "title": "", "live": true})];
        let disk = vec![json!({"id": "abc-123", "title": "Hello", "live": false})];
        let merged = merge_live(live, disk);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["title"], "Hello");
        assert_eq!(merged[0]["live"], true);
    }

    #[test]
    fn history_reads_chat_history_and_strips_user_query() {
        let root = std::env::temp_dir().join(format!(
            "gtm-hub-hist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("sess-hist");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("chat_history.jsonl"),
            concat!(
                r#"{"type":"system","content":"ignore"}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"noise <user_query>hello mars</user_query>"}]}"#,
                "\n",
                r#"{"type":"assistant","content":"ack","tool_calls":[{"name":"shell","arguments":"ls"}]}"#,
                "\n"
            ),
        )
        .unwrap();
        let msgs = session_history_in(&dir, 50);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["text"], "hello mars");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["toolTitle"], "shell");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["text"], "ack");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn history_falls_back_to_updates_jsonl() {
        let root = std::env::temp_dir().join(format!(
            "gtm-hub-upd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("sess-upd");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("updates.jsonl"),
            concat!(
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","text":"hi"}}}"#,
                "\n",
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"there"}}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let msgs = session_history_in(&dir, 50);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["text"], "hi");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["text"], "there");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn history_keeps_tool_diff_from_updates() {
        let root = std::env::temp_dir().join(format!(
            "gtm-hub-diff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("sess-diff");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("updates.jsonl"),
            concat!(
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","title":"Edit","status":"pending"}}}"#,
                "\n",
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","status":"completed","content":[{"type":"diff","path":"A.swift","oldText":"old","newText":"new"}]}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let msgs = session_history_in(&dir, 50);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["toolTitle"], "Edit");
        assert_eq!(msgs[0]["diffPath"], "A.swift");
        assert_eq!(msgs[0]["oldText"], "old");
        assert_eq!(msgs[0]["newText"], "new");
        assert_eq!(msgs[0]["text"], "completed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn history_keeps_tail_when_over_limit() {
        let root = std::env::temp_dir().join(format!(
            "gtm-hub-lim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("sess-lim");
        fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for i in 0..10 {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"content\":\"msg-{i}\"}}\n"
            ));
        }
        fs::write(dir.join("chat_history.jsonl"), body).unwrap();
        let msgs = session_history_in(&dir, 3);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["text"], "msg-7");
        assert_eq!(msgs[2]["text"], "msg-9");
        let _ = fs::remove_dir_all(&root);
    }
}
