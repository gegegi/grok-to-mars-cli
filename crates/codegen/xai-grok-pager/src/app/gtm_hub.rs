//! Optional GTM hub bridge: TUI hosts the live session so a phone prompt
//! runs in this process and `session/update` fans back out.
//!
//! GTM overlay: hub-tui — this file is fork-added inside an upstream crate.

use serde_json::Value;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum GtmHubInbound {
    Prompt {
        rpc_id: Value,
        session_id: String,
        text: String,
    },
    Cancel {
        session_id: String,
    },
}

#[derive(Debug)]
pub enum GtmHubOutbound {
    Host {
        session_id: String,
        cwd: String,
        title: String,
    },
    Unhost {
        session_id: String,
    },
    SessionUpdate {
        session_id: String,
        params: Value,
    },
    PromptResult {
        rpc_id: Value,
        result: Value,
        error: Option<String>,
    },
}

pub struct GtmHubBridge {
    pub inbound: mpsc::UnboundedReceiver<GtmHubInbound>,
    pub outbound: mpsc::UnboundedSender<GtmHubOutbound>,
}

pub fn extract_prompt_text(params: &Value) -> String {
    if let Some(s) = params.get("prompt").and_then(|v| v.as_str()) {
        return s.trim().to_string();
    }
    let Some(arr) = params.get("prompt").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    out.trim().to_string()
}
