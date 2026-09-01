//! One live ACP agent process per session.

use crate::frame::MAX_FRAME;
use crate::hub::{AgentSpawn, Hub};
use crate::rpc::{RpcError, rpc_error, rpc_result};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;

pub const SPAWN_FAILED: i64 = -32011;
pub const ALREADY_RESOLVED: i64 = -32013;

#[derive(Clone)]
pub struct ActorHandle {
    pub tx: mpsc::UnboundedSender<ActorCmd>,
}

pub enum ActorCmd {
    Rpc {
        client_id: u64,
        id: Value,
        method: String,
        params: Value,
    },
    Notify {
        method: String,
        params: Value,
    },
    ClientReply {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    Shutdown,
}

pub enum Start {
    New,
    Load,
    Resume,
}

struct AgentConn {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
    next_id: i64,
}

struct Pending {
    client_id: u64,
    rpc_id: Value,
    kind: PendingKind,
}

enum PendingKind {
    Prompt,
    Other,
}

struct QueuedPrompt {
    client_id: u64,
    rpc_id: Value,
    params: Value,
}

struct PermState {
    agent_id: Value,
    session_id: String,
    /// Hub-generated JSON-RPC id string → client.
    client_ids: HashMap<String, u64>,
    resolved: bool,
    dangerous: bool,
}

impl ActorHandle {
    pub fn spawn(
        hub: Hub,
        start: Start,
        origin_client: u64,
        origin_id: Value,
        params: Value,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Self { tx: tx.clone() };
        let handle_for_task = handle.clone();
        tokio::spawn(async move {
            actor_main(
                hub,
                rx,
                handle_for_task,
                start,
                origin_client,
                origin_id,
                params,
            )
            .await;
        });
        handle
    }

    pub fn send(&self, cmd: ActorCmd) {
        let _ = self.tx.send(cmd);
    }
}

async fn actor_main(
    hub: Hub,
    mut cmds: mpsc::UnboundedReceiver<ActorCmd>,
    handle: ActorHandle,
    start: Start,
    origin_client: u64,
    origin_id: Value,
    params: Value,
) {
    let fail_sid = params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Err(err) = actor_main_inner(
        &hub,
        &mut cmds,
        handle,
        start,
        origin_client,
        origin_id.clone(),
        params,
    )
    .await
    {
        eprintln!("session actor failed: {err:#}");
        tracing::warn!(error = %err, "session actor failed");
        hub.fail_session(
            fail_sid.as_deref(),
            origin_client,
            origin_id,
            RpcError::new(SPAWN_FAILED, format!("{err:#}")),
        );
    }
}

async fn actor_main_inner(
    hub: &Hub,
    cmds: &mut mpsc::UnboundedReceiver<ActorCmd>,
    handle: ActorHandle,
    start: Start,
    origin_client: u64,
    origin_id: Value,
    params: Value,
) -> Result<()> {
    let mut agent = spawn_agent(&hub.agent_spawn()).await?;

    drain_stderr(&mut agent);

    let init_id = agent.next_rpc_id();
    agent
        .write_msg(&json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false }
                },
                "clientInfo": {
                    "name": "gtm-hub",
                    "title": "Grok to Mars hub",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }))
        .await?;
    wait_response(&mut agent, init_id).await?;

    let method = match start {
        Start::New => "session/new",
        Start::Load => "session/load",
        Start::Resume => "session/resume",
    };
    let known_sid = params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(sid) = known_sid.as_deref() {
        hub.subscribe(origin_client, sid);
    }

    let boot_id = agent.next_rpc_id();
    agent
        .write_msg(&json!({
            "jsonrpc": "2.0",
            "id": boot_id,
            "method": method,
            "params": params,
        }))
        .await?;

    let mut session_id = known_sid;
    let mut buffered_updates: Vec<Value> = Vec::new();
    let boot_result = loop {
        let msg = agent.read_msg().await?;
        if msg.get("id") == Some(&json!(boot_id)) {
            if let Some(err) = msg.get("error") {
                bail!(
                    "{}",
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("session boot failed")
                );
            }
            break msg.get("result").cloned().unwrap_or(json!({}));
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("session/update") {
            if session_id.is_none()
                && let Some(sid) = msg
                    .pointer("/params/sessionId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            {
                session_id = Some(sid);
            }
            buffered_updates.push(msg);
            continue;
        }
        tracing::debug!(target: "gtm_hub::actor", "drop during boot: {msg}");
    };

    let sid = session_id
        .or_else(|| {
            boot_result
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| anyhow::anyhow!("agent {method} missing sessionId"))?;

    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let title = boot_result
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| crate::sessions::lookup_title(&sid))
        .unwrap_or_default();

    let origin = match start {
        Start::New => Some((origin_client, origin_id)),
        Start::Load | Start::Resume => None,
    };
    hub.complete_session(&sid, cwd, title, boot_result, handle.clone(), origin);

    for msg in buffered_updates {
        hub.broadcast_session(&sid, msg);
    }

    let mut pending: HashMap<i64, Pending> = HashMap::new();
    let mut prompt_queue: VecDeque<QueuedPrompt> = VecDeque::new();
    let mut prompt_inflight = false;
    let mut perm: Option<PermState> = None;

    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    ActorCmd::Shutdown => break,
                    ActorCmd::Notify { method, params } => {
                        let _ = agent
                            .write_msg(&json!({
                                "jsonrpc": "2.0",
                                "method": to_agent_method(&method),
                                "params": params,
                            }))
                            .await;
                    }
                    ActorCmd::ClientReply { id, result, error } => {
                        handle_perm_reply(
                            hub,
                            &mut agent,
                            &mut perm,
                            id,
                            result,
                            error,
                        )
                        .await;
                    }
                    ActorCmd::Rpc { client_id, id, method, params } => {
                        if method == "session/prompt" {
                            if prompt_inflight {
                                prompt_queue.push_back(QueuedPrompt { client_id, rpc_id: id, params });
                                continue;
                            }
                            prompt_inflight = true;
                            let rpc_id = id.clone();
                            if let Err(err) = forward_rpc(
                                &mut agent,
                                &mut pending,
                                client_id,
                                id,
                                &method,
                                params,
                                PendingKind::Prompt,
                            )
                            .await
                            {
                                prompt_inflight = false;
                                hub.send_to(
                                    client_id,
                                    RpcError::new(SPAWN_FAILED, err.to_string()).to_msg(rpc_id),
                                );
                            }
                        } else {
                            let reply_empty = method == "session/cancel";
                            let rpc_id = id.clone();
                            if let Err(err) = forward_rpc(
                                &mut agent,
                                &mut pending,
                                client_id,
                                id,
                                &method,
                                params,
                                PendingKind::Other,
                            )
                            .await
                            {
                                hub.send_to(
                                    client_id,
                                    RpcError::new(SPAWN_FAILED, err.to_string()).to_msg(rpc_id.clone()),
                                );
                            } else if reply_empty {
                                // ACP cancel is a notification on the agent; still ack the hub client.
                                pending.retain(|_, p| p.rpc_id != rpc_id);
                                hub.send_to(client_id, rpc_result(rpc_id, json!({})));
                            }
                        }
                    }
                }
            }
            msg = agent.read_msg() => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(err) => {
                        tracing::warn!(error = %err, session_id = %sid, "agent stdout closed");
                        fail_pending(hub, &pending, "agent process closed");
                        hub.remove_session(&sid);
                        break;
                    }
                };
                handle_agent_msg(
                    hub,
                    &mut agent,
                    &sid,
                    &mut pending,
                    &mut prompt_queue,
                    &mut prompt_inflight,
                    &mut perm,
                    msg,
                )
                .await;
            }
        }
    }

    let _ = agent.child.kill().await;
    Ok(())
}

async fn forward_rpc(
    agent: &mut AgentConn,
    pending: &mut HashMap<i64, Pending>,
    client_id: u64,
    rpc_id: Value,
    method: &str,
    params: Value,
    kind: PendingKind,
) -> Result<()> {
    let agent_id = agent.next_rpc_id();
    let is_cancel = method == "session/cancel";
    let msg = if is_cancel {
        json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": params,
        })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": agent_id,
            "method": to_agent_method(method),
            "params": params,
        })
    };
    agent.write_msg(&msg).await?;
    if !is_cancel {
        pending.insert(
            agent_id,
            Pending {
                client_id,
                rpc_id,
                kind,
            },
        );
    }
    Ok(())
}

async fn handle_perm_reply(
    hub: &Hub,
    agent: &mut AgentConn,
    perm: &mut Option<PermState>,
    id: Value,
    result: Option<Value>,
    error: Option<Value>,
) {
    let Some(state) = perm.as_mut() else { return };
    let key = id.as_str().unwrap_or("").to_string();
    if !state.client_ids.contains_key(&key) {
        return;
    }
    if state.resolved {
        return;
    }
    let winner_cid = *state.client_ids.get(&key).unwrap_or(&0);
    if state.dangerous && hub.is_lan(winner_cid) {
        hub.send_to(
            winner_cid,
            rpc_error(
                id,
                ALREADY_RESOLVED,
                "dangerous tools need a local Mac/TUI confirmation",
            ),
        );
        return;
    }
    state.resolved = true;
    let winner = key.clone();
    if let Some(result) = result {
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": state.agent_id,
                "result": result,
            }))
            .await;
    } else if let Some(error) = error {
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": state.agent_id,
                "error": error,
            }))
            .await;
    } else {
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": state.agent_id,
                "result": { "outcome": { "outcome": "cancelled" } },
            }))
            .await;
    }
    let others: Vec<(String, u64)> = state
        .client_ids
        .iter()
        .filter(|(k, _)| *k != &winner)
        .map(|(k, c)| (k.clone(), *c))
        .collect();
    for (hid, cid) in others {
        hub.send_to(
            cid,
            rpc_error(Value::String(hid), ALREADY_RESOLVED, "already_resolved"),
        );
    }
    hub.broadcast_session(
        &state.session_id,
        json!({
            "jsonrpc": "2.0",
            "method": "gtm/permission.resolved",
            "params": { "sessionId": state.session_id },
        }),
    );
    hub.clear_perm_routes(state.client_ids.keys());
}

#[allow(clippy::too_many_arguments)]
async fn handle_agent_msg(
    hub: &Hub,
    agent: &mut AgentConn,
    sid: &str,
    pending: &mut HashMap<i64, Pending>,
    prompt_queue: &mut VecDeque<QueuedPrompt>,
    prompt_inflight: &mut bool,
    perm: &mut Option<PermState>,
    msg: Value,
) {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let agent_id = msg.get("id").cloned();

    if !method.is_empty()
        && let Some(agent_id) = agent_id.clone()
    {
        if method == "session/request_permission" || method == "session/requestPermission" {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            start_permission(hub, agent, perm, sid, agent_id, params).await;
            return;
        }
        // Thin client: do not implement tools. Unblock the agent.
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "result": {},
            }))
            .await;
        return;
    }

    if !method.is_empty() {
        let mut out = msg;
        if let Some(p) = out.get_mut("params")
            && p.get("sessionId").is_none()
        {
            if let Some(obj) = p.as_object_mut() {
                obj.insert("sessionId".into(), json!(sid));
            }
        }
        hub.broadcast_session(sid, out);
        return;
    }

    let Some(id_val) = agent_id else { return };
    let aid = match id_val.as_i64() {
        Some(n) => n,
        None => return,
    };
    let Some(p) = pending.remove(&aid) else {
        return;
    };
    if matches!(p.kind, PendingKind::Prompt) {
        *prompt_inflight = false;
        hub.send_to(p.client_id, attach_id(msg, p.rpc_id));
        if let Some(next) = prompt_queue.pop_front() {
            *prompt_inflight = true;
            let _ = forward_rpc(
                agent,
                pending,
                next.client_id,
                next.rpc_id,
                "session/prompt",
                next.params,
                PendingKind::Prompt,
            )
            .await;
        }
        return;
    }
    hub.send_to(p.client_id, attach_id(msg, p.rpc_id));
}

fn attach_id(mut msg: Value, id: Value) -> Value {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("id".into(), id);
        obj.entry("jsonrpc").or_insert_with(|| json!("2.0"));
    }
    msg
}

async fn start_permission(
    hub: &Hub,
    agent: &mut AgentConn,
    perm: &mut Option<PermState>,
    sid: &str,
    agent_id: Value,
    params: Value,
) {
    let dangerous = is_dangerous_permission(&params);
    let subs = hub.subscribers(sid);
    if dangerous && !subs.iter().any(|cid| !hub.is_lan(*cid)) {
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "result": { "outcome": { "outcome": "cancelled" } },
            }))
            .await;
        hub.broadcast_session(
            sid,
            json!({
                "jsonrpc": "2.0",
                "method": "gtm/permission.resolved",
                "params": { "sessionId": sid, "reason": "no_local_client" },
            }),
        );
        return;
    }
    if subs.is_empty() {
        let _ = agent
            .write_msg(&json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "result": { "outcome": { "outcome": "cancelled" } },
            }))
            .await;
        return;
    }
    let mut client_ids = HashMap::new();
    for cid in subs {
        let hid = hub.next_perm_id();
        client_ids.insert(hid.clone(), cid);
        hub.register_perm_route(hid.clone(), sid);
        hub.send_to(
            cid,
            json!({
                "jsonrpc": "2.0",
                "id": hid,
                "method": "session/request_permission",
                "params": params,
            }),
        );
    }
    *perm = Some(PermState {
        agent_id,
        session_id: sid.to_string(),
        client_ids,
        resolved: false,
        dangerous,
    });
}

fn is_dangerous_permission(params: &Value) -> bool {
    let title = params
        .pointer("/toolCall/title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let kind = params
        .pointer("/toolCall/kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let blob = format!("{title} {kind}").to_lowercase();
    [
        "delete", "force", "push", "wipe", "drop ", "remove", "rm -rf",
    ]
    .iter()
    .any(|k| blob.contains(k))
}

fn fail_pending(hub: &Hub, pending: &HashMap<i64, Pending>, message: &str) {
    for p in pending.values() {
        hub.send_to(
            p.client_id,
            rpc_error(p.rpc_id.clone(), SPAWN_FAILED, message),
        );
    }
}

fn to_agent_method(method: &str) -> String {
    if let Some(rest) = method.strip_prefix("x.ai/") {
        format!("_x.ai/{rest}")
    } else {
        method.to_string()
    }
}

fn drain_stderr(agent: &mut AgentConn) {
    if let Some(err) = agent.child.stderr.take() {
        tokio::spawn(async move {
            let mut r = BufReader::new(err);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let t = line.trim_end();
                        if !t.is_empty() {
                            eprintln!("gtm-hub agent: {t}");
                            tracing::debug!(target: "gtm_hub::agent", "{t}");
                        }
                    }
                }
            }
        });
    }
}

async fn wait_response(agent: &mut AgentConn, id: i64) -> Result<Value> {
    loop {
        let msg = agent.read_msg().await?;
        if msg.get("id") == Some(&json!(id)) {
            if let Some(err) = msg.get("error") {
                bail!(
                    "{}",
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("agent error")
                );
            }
            return Ok(msg.get("result").cloned().unwrap_or(json!({})));
        }
    }
}

async fn spawn_agent(spawn: &AgentSpawn) -> Result<AgentConn> {
    let mut cmd = tokio::process::Command::new(&spawn.program);
    cmd.args(&spawn.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("PYTHONUNBUFFERED", "1");
    for (k, v) in child_env() {
        cmd.env(k, v);
    }
    #[allow(clippy::disallowed_methods)]
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn agent {}", spawn.program.display()))?;
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    Ok(AgentConn {
        stdin,
        stdout: BufReader::new(stdout),
        child,
        next_id: 1,
    })
}

impl AgentConn {
    fn next_rpc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn write_msg(&mut self, msg: &Value) -> Result<()> {
        let mut buf = serde_json::to_vec(msg)?;
        buf.push(b'\n');
        self.stdin.write_all(&buf).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_msg(&mut self) -> Result<Value> {
        loop {
            let mut buf = Vec::new();
            let n = self.stdout.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                bail!("agent closed stdout");
            }
            if buf.len() as u32 > MAX_FRAME {
                bail!("agent line too large");
            }
            let s = String::from_utf8_lossy(&buf);
            let t = s.trim();
            if t.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(t) {
                Ok(v) => return Ok(v),
                Err(_) => tracing::debug!(target: "gtm_hub::agent", "non-json stdout: {t}"),
            }
        }
    }
}

fn child_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if std::env::var_os("GROK_CURSOR_MCPS_ENABLED").is_none() {
        env.push(("GROK_CURSOR_MCPS_ENABLED".into(), "0".into()));
    }
    if std::env::var_os("GROK_CLAUDE_MCPS_ENABLED").is_none() {
        env.push(("GROK_CLAUDE_MCPS_ENABLED".into(), "0".into()));
    }
    let mut path_parts: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        path_parts.push(home.join(".grok").join("bin"));
        path_parts.push(home.join(".local").join("bin"));
    }
    path_parts.push(PathBuf::from("/opt/homebrew/bin"));
    path_parts.push(PathBuf::from("/usr/local/bin"));
    path_parts.push(PathBuf::from("/usr/bin"));
    path_parts.push(PathBuf::from("/bin"));
    if let Some(existing) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&existing) {
            if !path_parts.contains(&p) {
                path_parts.push(p);
            }
        }
    }
    env.push((
        "PATH".into(),
        std::env::join_paths(&path_parts)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ));
    env
}
