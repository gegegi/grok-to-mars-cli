//! Shared hub state: clients, live session actors, routing.

use crate::actor::{ActorCmd, ActorHandle, Start};
use crate::rpc::{RpcError, rpc_result};
use crate::sessions::{list_disk_sessions, merge_live};
use crate::tls;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Debug)]
pub struct AgentSpawn {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl AgentSpawn {
    pub fn from_env() -> Self {
        if let Ok(program) = std::env::var("GTM_HUB_AGENT_BIN") {
            let args = std::env::var("GTM_HUB_AGENT_ARGS")
                .ok()
                .map(|s| {
                    s.split_whitespace()
                        .filter(|p| !p.is_empty())
                        .map(|p| p.to_string())
                        .collect()
                })
                .unwrap_or_default();
            return Self {
                program: PathBuf::from(program),
                args,
            };
        }
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gtm"));
        Self {
            program,
            args: vec!["agent".into(), "--no-leader".into(), "stdio".into()],
        }
    }
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

pub const LOCAL_CONN_MAX: usize = 8;
pub const LAN_CONN_MAX: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanWant {
    pub enabled: bool,
    pub port: u16,
}

struct Inner {
    spawn: AgentSpawn,
    clients: Mutex<HashMap<u64, Client>>,
    sessions: Mutex<HashMap<String, Entry>>,
    perm_routes: Mutex<HashMap<String, String>>,
    hosted_prompts: Mutex<HashMap<String, HostedPrompt>>,
    next_client: AtomicU64,
    next_perm: AtomicU64,
    lan_tx: watch::Sender<LanWant>,
}

struct Client {
    tx: mpsc::UnboundedSender<Value>,
    subs: HashSet<String>,
    lan: bool,
    device_fp: Option<String>,
}

enum Entry {
    Starting { waiters: Vec<(u64, Value)> },
    Live(Slot),
    /// Interactive TUI (or mac app) owns the live agent; hub only fans out.
    Hosted(HostSlot),
}

struct Slot {
    cwd: Option<String>,
    title: String,
    new_result: Value,
    actor: ActorHandle,
}

struct HostSlot {
    client_id: u64,
    cwd: Option<String>,
    title: String,
}

struct HostedPrompt {
    origin: u64,
    host: u64,
}

pub enum Dispatch {
    Reply(Result<Value, RpcError>),
    Pending,
}

impl Hub {
    pub fn new() -> Self {
        Self::with_agent(AgentSpawn::from_env())
    }

    pub fn with_agent(spawn: AgentSpawn) -> Self {
        let (lan_tx, _) = watch::channel(LanWant {
            enabled: false,
            port: tls::lan_port(),
        });
        Self {
            inner: Arc::new(Inner {
                spawn,
                clients: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                perm_routes: Mutex::new(HashMap::new()),
                hosted_prompts: Mutex::new(HashMap::new()),
                next_client: AtomicU64::new(1),
                next_perm: AtomicU64::new(1),
                lan_tx,
            }),
        }
    }

    pub fn subscribe_lan(&self) -> watch::Receiver<LanWant> {
        self.inner.lan_tx.subscribe()
    }

    pub fn lan_want(&self) -> LanWant {
        self.inner.lan_tx.borrow().clone()
    }

    pub fn is_lan(&self, client_id: u64) -> bool {
        self.inner
            .clients
            .lock()
            .unwrap()
            .get(&client_id)
            .map(|c| c.lan)
            .unwrap_or(false)
    }

    pub fn drop_device(&self, fp: &str) {
        let mut g = self.inner.clients.lock().unwrap();
        evict_fp(&mut g, fp, "revoked");
    }

    pub(crate) fn agent_spawn(&self) -> AgentSpawn {
        self.inner.spawn.clone()
    }

    pub fn hello(
        &self,
        params: &Value,
        tx: mpsc::UnboundedSender<Value>,
        lan: bool,
        device_fp: Option<String>,
    ) -> Result<(u64, Value), RpcError> {
        let kind = params
            .pointer("/client/kind")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !matches!(kind, "macos" | "tui" | "stdio" | "ios" | "android") {
            return Err(RpcError::new(-32602, format!("unknown client kind {kind}")));
        }
        if lan && !matches!(kind, "ios" | "android") {
            return Err(RpcError::new(
                -32000,
                "LAN listener only accepts ios/android clients",
            ));
        }
        if !lan && matches!(kind, "ios" | "android") {
            return Err(RpcError::new(
                -32000,
                "mobile clients must use the TLS listener",
            ));
        }
        if lan {
            let Some(fp) = device_fp.as_deref() else {
                return Err(RpcError::new(-32000, "missing device certificate"));
            };
            if tls::lookup_device_fp(fp).is_none() {
                return Err(RpcError::new(-32000, "device not enrolled or expired"));
            }
        }
        let version = params
            .pointer("/protocolVersion")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if version != crate::PROTOCOL_VERSION {
            return Err(RpcError::new(
                -32000,
                format!("unsupported protocolVersion {version}"),
            ));
        }
        let id = {
            let mut g = self.inner.clients.lock().unwrap();
            // Force-quit phones leave the TLS socket up until idle/keepalive.
            // Same enrolled device reconnecting must replace that zombie
            // *before* the LAN cap, or relaunch hits -32001.
            if lan {
                if let Some(fp) = device_fp.as_deref() {
                    evict_fp(&mut g, fp, "replaced");
                }
            }
            let n = g.values().filter(|c| c.lan == lan).count();
            let max = if lan { LAN_CONN_MAX } else { LOCAL_CONN_MAX };
            if n >= max {
                return Err(RpcError::new(-32001, "too many connections"));
            }
            let id = self.inner.next_client.fetch_add(1, Ordering::Relaxed);
            g.insert(
                id,
                Client {
                    tx,
                    subs: HashSet::new(),
                    lan,
                    device_fp,
                },
            );
            id
        };
        Ok((
            id,
            json!({
                "clientId": id,
                "hubVersion": env!("CARGO_PKG_VERSION"),
                "protocolVersion": crate::PROTOCOL_VERSION,
                "transport": if lan { "lan" } else { "unix" },
                "kind": kind,
            }),
        ))
    }

    pub fn handle_request(
        &self,
        client_id: u64,
        id: Value,
        method: &str,
        params: Value,
    ) -> Dispatch {
        if self.is_lan(client_id) && !lan_method_allowed(method) {
            return Dispatch::Reply(Err(RpcError::new(
                -32000,
                format!("{method} is not allowed on LAN"),
            )));
        }
        match method {
            "gtm/hello" => {
                Dispatch::Reply(Err(RpcError::new(-32000, "gtm/hello already completed")))
            }
            "gtm/ping" => Dispatch::Reply(Ok(json!({ "ok": true }))),
            "gtm/hub.info" => Dispatch::Reply(Ok(self.hub_info_value())),
            "gtm/remote.status" => Dispatch::Reply(Ok(self.remote_status_value())),
            "gtm/remote.on" => self.remote_on(client_id, params),
            "gtm/remote.off" => self.remote_off(client_id),
            "gtm/remote.revoke" => self.remote_revoke(client_id, params),
            "gtm/sessions.list" => Dispatch::Reply(Ok(json!({ "sessions": self.list_sessions() }))),
            "gtm/sessions.subscribe" => self.subscribe_req(client_id, params),
            "gtm/sessions.unsubscribe" => self.unsubscribe_req(client_id, params),
            "gtm/sessions.host" => self.host_req(client_id, params),
            "gtm/sessions.unhost" => self.unhost_req(client_id, params),
            "gtm/session.history" => self.session_history_req(params),
            "initialize" => Dispatch::Reply(Ok(json!({
                "protocolVersion": 1,
                "serverInfo": {
                    "name": "gtm-hub",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "agentInfo": {
                    "name": "gtm-hub",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "agentCapabilities": {
                    "loadSession": true,
                    "resumeSession": true,
                },
            }))),
            "session/new" => self.session_new(client_id, id, params),
            "session/load" => self.session_attach(client_id, id, Start::Load, params),
            "session/resume" => self.session_attach(client_id, id, Start::Resume, params),
            "session/prompt" => self.session_prompt(client_id, id, params),
            "session/cancel" => self.session_cancel(client_id, id, params),
            other if is_session_method(other) => {
                self.forward_to_actor(client_id, id, other, params)
            }
            other => Dispatch::Reply(Err(RpcError::new(
                -32601,
                format!("method not found: {other}"),
            ))),
        }
    }

    pub fn handle_notification(&self, client_id: u64, method: &str, params: Value) {
        if method == "session/update"
            || method == "x.ai/session/update"
            || method == "x.ai/session_notification"
        {
            let sid = params
                .get("sessionId")
                .or_else(|| params.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| self.hosted_sid_for_client(client_id));
            if let Some(sid) = sid {
                if self.is_host(client_id, &sid) || self.host_client(&sid).is_some() && !self.is_lan(client_id)
                {
                    let n = self.subscribers(&sid).len();
                    if n == 0 {
                        eprintln!("gtm-hub clone {sid}: no subscribers");
                    }
                    let mut params = params;
                    if params.get("sessionId").is_none() {
                        if let Some(obj) = params.as_object_mut() {
                            obj.insert("sessionId".into(), json!(sid));
                        }
                    }
                    self.broadcast_session(
                        &sid,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": params,
                        }),
                    );
                    return;
                }
            }
        }
        if (method == "session/cancel" || is_session_method(method))
            && let Some(sid) = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            && let Some(actor) = self.actor_for(&sid)
        {
            actor.send(ActorCmd::Notify {
                method: method.to_string(),
                params,
            });
        }
    }

    pub fn handle_client_reply(
        &self,
        _client_id: u64,
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    ) {
        let hosted_key = rpc_id_key(&id);
        if let Some(pending) = self.inner.hosted_prompts.lock().unwrap().remove(&hosted_key) {
            if let Some(error) = error {
                self.send_to(
                    pending.origin,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": error,
                    }),
                );
            } else {
                self.send_to(
                    pending.origin,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result.unwrap_or(json!({})),
                    }),
                );
            }
            return;
        }
        let key = id.as_str().unwrap_or("").to_string();
        let sid = self.inner.perm_routes.lock().unwrap().get(&key).cloned();
        let Some(sid) = sid else { return };
        if let Some(actor) = self.actor_for(&sid) {
            actor.send(ActorCmd::ClientReply { id, result, error });
        }
    }

    pub fn disconnect(&self, client_id: u64) {
        self.inner.clients.lock().unwrap().remove(&client_id);
        self.drop_host_client(client_id);
    }

    #[cfg(test)]
    fn lan_clients(&self) -> usize {
        self.inner
            .clients
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.lan)
            .count()
    }

    #[cfg(test)]
    fn insert_live_for_test(&self, sid: &str) -> mpsc::UnboundedReceiver<ActorCmd> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.sessions.lock().unwrap().insert(
            sid.to_string(),
            Entry::Live(Slot {
                cwd: Some("/tmp".into()),
                title: "live".into(),
                new_result: json!({ "sessionId": sid }),
                actor: ActorHandle { tx },
            }),
        );
        rx
    }

    #[cfg(test)]
    fn is_hosted(&self, sid: &str) -> bool {
        matches!(
            self.inner.sessions.lock().unwrap().get(sid),
            Some(Entry::Hosted(_))
        )
    }

    pub fn shutdown(&self) {
        let actors: Vec<ActorHandle> = {
            let mut g = self.inner.sessions.lock().unwrap();
            g.drain()
                .filter_map(|(_, e)| match e {
                    Entry::Live(s) => Some(s.actor),
                    Entry::Starting { .. } | Entry::Hosted(_) => None,
                })
                .collect()
        };
        for a in actors {
            a.send(ActorCmd::Shutdown);
        }
        self.inner.clients.lock().unwrap().clear();
    }

    pub fn send_to(&self, client_id: u64, msg: Value) {
        if let Some(c) = self.inner.clients.lock().unwrap().get(&client_id) {
            let _ = c.tx.send(msg);
        }
    }

    pub fn broadcast_session(&self, session_id: &str, msg: Value) {
        let clients = self.inner.clients.lock().unwrap();
        for c in clients.values() {
            if c.subs.contains(session_id) {
                let _ = c.tx.send(msg.clone());
            }
        }
    }

    pub fn broadcast_all(&self, msg: Value) {
        let clients = self.inner.clients.lock().unwrap();
        for c in clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    pub fn subscribe(&self, client_id: u64, session_id: &str) {
        if let Some(c) = self.inner.clients.lock().unwrap().get_mut(&client_id) {
            c.subs.insert(session_id.to_string());
        }
    }

    pub fn subscribers(&self, session_id: &str) -> Vec<u64> {
        self.inner
            .clients
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| c.subs.contains(session_id))
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn next_perm_id(&self) -> String {
        let n = self.inner.next_perm.fetch_add(1, Ordering::Relaxed);
        format!("hp-{n}")
    }

    pub fn register_perm_route(&self, hid: String, session_id: &str) {
        self.inner
            .perm_routes
            .lock()
            .unwrap()
            .insert(hid, session_id.to_string());
    }

    pub fn clear_perm_routes<'a>(&self, ids: impl Iterator<Item = &'a String>) {
        let mut g = self.inner.perm_routes.lock().unwrap();
        for id in ids {
            g.remove(id);
        }
    }

    pub fn complete_session(
        &self,
        sid: &str,
        cwd: Option<String>,
        title: String,
        result: Value,
        actor: ActorHandle,
        origin: Option<(u64, Value)>,
    ) {
        let waiters = {
            let mut g = self.inner.sessions.lock().unwrap();
            if matches!(g.get(sid), Some(Entry::Hosted(_))) {
                drop(g);
                eprintln!("gtm-hub actor boot dropped; TUI already hosts {sid}");
                actor.send(ActorCmd::Shutdown);
                return;
            }
            let waiters = match g.remove(sid) {
                Some(Entry::Starting { waiters }) => waiters,
                Some(keep @ Entry::Live(_)) => {
                    g.insert(sid.to_string(), keep);
                    Vec::new()
                }
                Some(hosted @ Entry::Hosted(_)) => {
                    g.insert(sid.to_string(), hosted);
                    drop(g);
                    actor.send(ActorCmd::Shutdown);
                    return;
                }
                None => Vec::new(),
            };
            g.insert(
                sid.to_string(),
                Entry::Live(Slot {
                    cwd,
                    title,
                    new_result: result.clone(),
                    actor,
                }),
            );
            waiters
        };
        let mut replies = waiters;
        if let Some(o) = origin
            && !replies.iter().any(|(c, i)| *c == o.0 && *i == o.1)
        {
            replies.push(o);
        }
        for (cid, id) in replies {
            self.subscribe(cid, sid);
            self.send_to(cid, rpc_result(id, result.clone()));
        }
        self.broadcast_all(json!({
            "jsonrpc": "2.0",
            "method": "gtm/sessions.changed",
            "params": { "id": sid, "live": true },
        }));
    }

    pub fn fail_session(
        &self,
        sid: Option<&str>,
        origin_client: u64,
        origin_id: Value,
        err: RpcError,
    ) {
        let mut waiters = vec![(origin_client, origin_id)];
        if let Some(sid) = sid {
            let mut g = self.inner.sessions.lock().unwrap();
            if let Some(Entry::Starting { waiters: w }) = g.remove(sid) {
                waiters = w;
            }
        }
        let mut seen = HashSet::new();
        for (cid, id) in waiters {
            if seen.insert(format!("{cid}:{id}")) {
                self.send_to(cid, err.to_msg(id));
            }
        }
    }

    pub fn remove_session(&self, sid: &str) {
        self.inner.sessions.lock().unwrap().remove(sid);
        self.broadcast_all(json!({
            "jsonrpc": "2.0",
            "method": "gtm/sessions.changed",
            "params": { "id": sid, "live": false, "reason": "actor_exit" },
        }));
    }

    fn list_sessions(&self) -> Vec<Value> {
        let live: Vec<Value> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| match e {
                Entry::Live(s) => json!({
                    "id": id,
                    "title": s.title,
                    "live": true,
                    "hosted": false,
                    "cwd": s.cwd,
                }),
                Entry::Hosted(s) => json!({
                    "id": id,
                    "title": s.title,
                    "live": true,
                    "hosted": true,
                    "cwd": s.cwd,
                }),
                Entry::Starting { .. } => json!({
                    "id": id,
                    "title": "",
                    "live": true,
                    "cwd": Value::Null,
                }),
            })
            .collect();
        merge_live(live, list_disk_sessions())
    }

    fn host_req(&self, client_id: u64, params: Value) -> Dispatch {
        if self.is_lan(client_id) {
            return Dispatch::Reply(Err(RpcError::new(
                -32000,
                "gtm/sessions.host is local-only",
            )));
        }
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
        else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "sessionId required")));
        };
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| crate::sessions::lookup_cwd(&sid));
        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| crate::sessions::lookup_title(&sid))
            .unwrap_or_default();
        let mut waiters: Vec<(u64, Value)> = Vec::new();
        let mut takeover: Option<ActorHandle> = None;
        let mut replaced_host: Option<u64> = None;
        {
            let mut g = self.inner.sessions.lock().unwrap();
            match g.remove(&sid) {
                Some(Entry::Hosted(h)) if h.client_id != client_id => {
                    // Previous TUI died or resumed; the new process takes over.
                    eprintln!(
                        "gtm-hub TUI host replace {sid} {} -> {client_id}",
                        h.client_id
                    );
                    replaced_host = Some(h.client_id);
                }
                Some(Entry::Live(slot)) => {
                    takeover = Some(slot.actor);
                }
                Some(Entry::Starting { waiters: w }) => {
                    waiters = w;
                }
                _ => {}
            }
            g.insert(
                sid.clone(),
                Entry::Hosted(HostSlot {
                    client_id,
                    cwd: cwd.clone(),
                    title: title.clone(),
                }),
            );
        }
        let took_over = takeover.is_some();
        if let Some(old) = replaced_host {
            self.fail_hosted_prompts_for_host(old);
        }
        if let Some(actor) = takeover {
            eprintln!("gtm-hub TUI took over actor for {sid}");
            actor.send(ActorCmd::Shutdown);
        } else if replaced_host.is_none() {
            eprintln!("gtm-hub TUI hosts {sid} (client {client_id})");
        }
        let hosted_result = json!({
            "sessionId": sid,
            "title": title,
            "cwd": cwd,
        });
        for (cid, id) in waiters {
            self.subscribe(cid, &sid);
            self.send_to(cid, rpc_result(id, hosted_result.clone()));
        }
        self.broadcast_all(json!({
            "jsonrpc": "2.0",
            "method": "gtm/sessions.changed",
            "params": { "id": sid, "live": true, "hosted": true },
        }));
        Dispatch::Reply(Ok(json!({
            "ok": true,
            "sessionId": sid,
            "cwd": cwd,
            "title": title,
            "tookOver": took_over,
        })))
    }

    fn unhost_req(&self, client_id: u64, params: Value) -> Dispatch {
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "sessionId required")));
        };
        self.unhost_if_owner(client_id, sid);
        Dispatch::Reply(Ok(json!({ "ok": true, "sessionId": sid })))
    }

    fn session_prompt(&self, client_id: u64, id: Value, params: Value) -> Dispatch {
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        else {
            return Dispatch::Reply(Err(RpcError::new(
                -32602,
                "session/prompt requires sessionId",
            )));
        };
        if let Some(host) = self.host_client(&sid) {
            if host == client_id {
                return Dispatch::Reply(Err(RpcError::new(
                    -32000,
                    "host client already owns this session",
                )));
            }
            // Prompt forwarding does not require a prior subscribe; output
            // fan-out does. Always subscribe the origin so TUI clones arrive.
            self.subscribe(client_id, &sid);
            self.inner.hosted_prompts.lock().unwrap().insert(
                rpc_id_key(&id),
                HostedPrompt {
                    origin: client_id,
                    host,
                },
            );
            if let Some(text) = extract_prompt_text(&params) {
                self.broadcast_session(
                    &sid,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "user_message_chunk",
                                "content": { "type": "text", "text": text }
                            }
                        }
                    }),
                );
            }
            eprintln!("gtm-hub forward prompt {sid} -> TUI host {host}");
            self.send_to(
                host,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "session/prompt",
                    "params": params,
                }),
            );
            return Dispatch::Pending;
        }
        self.forward_to_actor(client_id, id, "session/prompt", params)
    }

    fn session_cancel(&self, client_id: u64, id: Value, params: Value) -> Dispatch {
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        else {
            return Dispatch::Reply(Err(RpcError::new(
                -32602,
                "session/cancel requires sessionId",
            )));
        };
        if let Some(host) = self.host_client(&sid) {
            self.send_to(
                host,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": params,
                }),
            );
            return Dispatch::Reply(Ok(json!({})));
        }
        self.forward_to_actor(client_id, id, "session/cancel", params)
    }

    fn host_client(&self, sid: &str) -> Option<u64> {
        match self.inner.sessions.lock().unwrap().get(sid) {
            Some(Entry::Hosted(h)) => Some(h.client_id),
            _ => None,
        }
    }

    fn is_host(&self, client_id: u64, sid: &str) -> bool {
        self.host_client(sid) == Some(client_id)
    }

    fn hosted_sid_for_client(&self, client_id: u64) -> Option<String> {
        let g = self.inner.sessions.lock().unwrap();
        let mut found = None;
        for (id, e) in g.iter() {
            if let Entry::Hosted(h) = e {
                if h.client_id == client_id {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(id.clone());
                }
            }
        }
        found
    }

    fn unhost_if_owner(&self, client_id: u64, sid: &str) {
        let removed = {
            let mut g = self.inner.sessions.lock().unwrap();
            match g.get(sid) {
                Some(Entry::Hosted(h)) if h.client_id == client_id => {
                    g.remove(sid);
                    true
                }
                _ => false,
            }
        };
        if removed {
            self.fail_hosted_prompts_for_host(client_id);
            self.broadcast_all(json!({
                "jsonrpc": "2.0",
                "method": "gtm/sessions.changed",
                "params": { "id": sid, "live": false, "reason": "unhost" },
            }));
        }
    }

    fn drop_host_client(&self, client_id: u64) {
        let sids: Vec<String> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, e)| match e {
                Entry::Hosted(h) if h.client_id == client_id => Some(id.clone()),
                _ => None,
            })
            .collect();
        for sid in sids {
            self.unhost_if_owner(client_id, &sid);
        }
        self.fail_hosted_prompts_for_host(client_id);
    }

    fn fail_hosted_prompts_for_host(&self, host: u64) {
        let pending: Vec<(String, HostedPrompt)> = {
            let mut g = self.inner.hosted_prompts.lock().unwrap();
            let keys: Vec<String> = g
                .iter()
                .filter(|(_, p)| p.host == host)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| g.remove(&k).map(|p| (k, p)))
                .collect()
        };
        for (key, p) in pending {
            let id = serde_json::from_str(&key).unwrap_or(json!(null));
            self.send_to(
                p.origin,
                RpcError::new(-32012, "TUI session host disconnected").to_msg(id),
            );
        }
    }

    fn session_history_req(&self, params: Value) -> Dispatch {
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "sessionId required")));
        };
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(crate::sessions::HISTORY_DEFAULT as u64) as usize;
        let messages = crate::sessions::session_history(sid, limit);
        Dispatch::Reply(Ok(json!({
            "sessionId": sid,
            "messages": messages,
        })))
    }

    fn subscribe_req(&self, client_id: u64, params: Value) -> Dispatch {
        let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "subscribe requires sessionId")));
        };
        self.subscribe(client_id, sid);
        let live = self.actor_for(sid).is_some() || self.host_client(sid).is_some();
        Dispatch::Reply(Ok(json!({ "ok": true, "live": live })))
    }

    fn unsubscribe_req(&self, client_id: u64, params: Value) -> Dispatch {
        let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "unsubscribe requires sessionId")));
        };
        if let Some(c) = self.inner.clients.lock().unwrap().get_mut(&client_id) {
            c.subs.remove(sid);
        }
        Dispatch::Reply(Ok(json!({ "ok": true })))
    }

    fn session_new(&self, client_id: u64, id: Value, params: Value) -> Dispatch {
        if params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            return Dispatch::Reply(Err(RpcError::new(-32602, "session/new requires cwd")));
        }
        ActorHandle::spawn(self.clone(), Start::New, client_id, id, params);
        Dispatch::Pending
    }

    fn session_attach(&self, client_id: u64, id: Value, start: Start, params: Value) -> Dispatch {
        let Some(sid) = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
        else {
            return Dispatch::Reply(Err(RpcError::new(
                -32602,
                "session/load requires sessionId",
            )));
        };
        let mut params = params;
        let cwd_empty = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty();
        if cwd_empty {
            if let Some(cwd) = crate::sessions::lookup_cwd(&sid) {
                params["cwd"] = json!(cwd);
            }
        }
        if params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            return Dispatch::Reply(Err(RpcError::new(
                -32602,
                format!("session/load {sid} is missing cwd"),
            )));
        }
        let mut g = self.inner.sessions.lock().unwrap();
        match g.get_mut(&sid) {
            Some(Entry::Live(slot)) => {
                let result = slot.new_result.clone();
                drop(g);
                self.subscribe(client_id, &sid);
                Dispatch::Reply(Ok(result))
            }
            Some(Entry::Hosted(slot)) => {
                let result = json!({
                    "sessionId": sid,
                    "title": slot.title,
                    "cwd": slot.cwd,
                    "hosted": true,
                });
                drop(g);
                eprintln!("gtm-hub attach {sid} to TUI host");
                self.subscribe(client_id, &sid);
                Dispatch::Reply(Ok(result))
            }
            Some(Entry::Starting { waiters }) => {
                waiters.push((client_id, id));
                Dispatch::Pending
            }
            None => {
                eprintln!("gtm-hub spawn actor {sid} (no TUI host)");
                g.insert(
                    sid,
                    Entry::Starting {
                        waiters: vec![(client_id, id.clone())],
                    },
                );
                drop(g);
                ActorHandle::spawn(self.clone(), start, client_id, id, params);
                Dispatch::Pending
            }
        }
    }

    fn forward_to_actor(&self, client_id: u64, id: Value, method: &str, params: Value) -> Dispatch {
        let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) else {
            return Dispatch::Reply(Err(RpcError::new(
                -32602,
                format!("{method} requires sessionId"),
            )));
        };
        let Some(actor) = self.actor_for(sid) else {
            return Dispatch::Reply(Err(RpcError::new(-32012, format!("no live session {sid}"))));
        };
        actor.send(ActorCmd::Rpc {
            client_id,
            id,
            method: method.to_string(),
            params,
        });
        Dispatch::Pending
    }

    fn actor_for(&self, sid: &str) -> Option<ActorHandle> {
        match self.inner.sessions.lock().unwrap().get(sid) {
            Some(Entry::Live(s)) => Some(s.actor.clone()),
            _ => None,
        }
    }

    fn hub_info_value(&self) -> Value {
        let lan = self.lan_want();
        let live: Vec<Value> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| match e {
                Entry::Live(s) => json!({
                    "id": id,
                    "hosted": false,
                    "title": s.title,
                    "cwd": s.cwd,
                }),
                Entry::Hosted(s) => json!({
                    "id": id,
                    "hosted": true,
                    "title": s.title,
                    "cwd": s.cwd,
                }),
                Entry::Starting { .. } => json!({
                    "id": id,
                    "hosted": false,
                    "starting": true,
                }),
            })
            .collect();
        json!({
            "pid": std::process::id(),
            "socket": crate::paths::socket_path().display().to_string(),
            "protocolVersion": crate::PROTOCOL_VERSION,
            "running": true,
            "lan": lan.enabled,
            "lanBind": lan.enabled.then(|| format!("0.0.0.0:{}", lan.port)),
            "live": live,
        })
    }

    fn remote_status_value(&self) -> Value {
        let lan = self.lan_want();
        json!({
            "lan": lan.enabled,
            "port": lan.port,
            "bind": lan.enabled.then(|| format!("0.0.0.0:{}", lan.port)),
            "devices": tls::list_devices().len(),
            "host": tls::guess_lan_host(),
        })
    }

    fn remote_on(&self, client_id: u64, params: Value) -> Dispatch {
        if self.is_lan(client_id) {
            return Dispatch::Reply(Err(RpcError::new(-32000, "gtm/remote.on is local-only")));
        }
        let port = params
            .get("port")
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok())
            .filter(|p| *p > 0)
            .unwrap_or_else(tls::lan_port);
        if let Err(err) = tls::ensure_server_materials() {
            return Dispatch::Reply(Err(RpcError::new(-32011, err.to_string())));
        }
        let _ = self.inner.lan_tx.send(LanWant {
            enabled: true,
            port,
        });
        Dispatch::Reply(Ok(json!({
            "lan": true,
            "port": port,
            "bind": format!("0.0.0.0:{port}"),
            "host": tls::guess_lan_host(),
            "alpn": tls::ALPN,
        })))
    }

    fn remote_off(&self, client_id: u64) -> Dispatch {
        if self.is_lan(client_id) {
            return Dispatch::Reply(Err(RpcError::new(-32000, "gtm/remote.off is local-only")));
        }
        let port = self.lan_want().port;
        let _ = self.inner.lan_tx.send(LanWant {
            enabled: false,
            port,
        });
        Dispatch::Reply(Ok(json!({ "lan": false })))
    }

    fn remote_revoke(&self, client_id: u64, params: Value) -> Dispatch {
        if self.is_lan(client_id) {
            return Dispatch::Reply(Err(RpcError::new(
                -32000,
                "gtm/remote.revoke is local-only",
            )));
        }
        let Some(id) = params.get("deviceId").and_then(|v| v.as_str()) else {
            return Dispatch::Reply(Err(RpcError::new(-32602, "deviceId required")));
        };
        let rec = tls::list_devices().into_iter().find(|d| d.device_id == id);
        match tls::revoke_device(id) {
            Ok(true) => {
                if let Some(rec) = rec {
                    self.drop_device(&rec.cert_sha256);
                }
                Dispatch::Reply(Ok(json!({ "ok": true, "deviceId": id })))
            }
            Ok(false) => Dispatch::Reply(Err(RpcError::new(-32602, "unknown device"))),
            Err(err) => Dispatch::Reply(Err(RpcError::new(-32011, err.to_string()))),
        }
    }
}

fn rpc_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn extract_prompt_text(params: &Value) -> Option<String> {
    let prompt = params.get("prompt")?;
    if let Some(s) = prompt.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    let arr = prompt.as_array()?;
    let mut out = String::new();
    for block in arr {
        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    let t = out.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn evict_fp(g: &mut HashMap<u64, Client>, fp: &str, reason: &str) {
    let ids: Vec<u64> = g
        .iter()
        .filter(|(_, c)| c.device_fp.as_deref() == Some(fp))
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        if let Some(c) = g.remove(&id) {
            let _ = c.tx.send(json!({
                "jsonrpc": "2.0",
                "method": "gtm/hub.shutdown",
                "params": { "reason": reason },
            }));
        }
    }
}

fn is_session_method(method: &str) -> bool {
    method.starts_with("session/") || method.starts_with("x.ai/") || method.starts_with("_x.ai/")
}

fn lan_method_allowed(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "gtm/ping"
            | "gtm/hub.info"
            | "gtm/sessions.list"
            | "gtm/sessions.subscribe"
            | "gtm/sessions.unsubscribe"
            | "gtm/session.history"
            | "session/load"
            | "session/resume"
            | "session/prompt"
            | "session/cancel"
            | "session/set_mode"
            | "session/set_model"
    ) || method.starts_with("x.ai/")
        || method.starts_with("_x.ai/")
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::paths::lock_test_home();
        let dir = std::env::temp_dir().join(format!(
            "gtm-hub-hello-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("GTM_HOME");
        unsafe { std::env::set_var("GTM_HOME", &dir) };
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("GTM_HOME", v) },
            None => unsafe { std::env::remove_var("GTM_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn ios_hello() -> Value {
        json!({
            "protocolVersion": 1,
            "client": { "kind": "ios", "name": "phone", "version": "0" },
        })
    }

    #[test]
    fn lan_same_device_hello_replaces_stale_connection() {
        with_tmp_home(|| {
            crate::tls::issue_device(86_400).expect("enroll");
            let fp = crate::tls::list_devices()[0].cert_sha256.clone();
            let hub = Hub::new();
            let params = ios_hello();
            let (tx1, mut rx1) = mpsc::unbounded_channel();
            let (a, _) = hub
                .hello(&params, tx1, true, Some(fp.clone()))
                .expect("first hello");
            let (tx2, _rx2) = mpsc::unbounded_channel();
            let (b, _) = hub
                .hello(&params, tx2, true, Some(fp.clone()))
                .expect("replace hello");
            assert_ne!(a, b);
            let msg = rx1.try_recv().expect("replaced notify");
            assert_eq!(msg["method"], "gtm/hub.shutdown");
            assert_eq!(msg["params"]["reason"], "replaced");
            assert_eq!(hub.lan_clients(), 1);
            for _ in 0..5 {
                let (tx, _rx) = mpsc::unbounded_channel();
                hub.hello(&params, tx, true, Some(fp.clone()))
                    .expect("repeat replace");
            }
            assert_eq!(hub.lan_clients(), 1);
        });
    }

    #[test]
    fn lan_cap_still_applies_across_distinct_devices() {
        with_tmp_home(|| {
            let hub = Hub::new();
            let params = ios_hello();
            let mut fps = Vec::new();
            for _ in 0..3 {
                let enroll = crate::tls::issue_device(86_400).unwrap();
                let rec = crate::tls::list_devices()
                    .into_iter()
                    .find(|d| d.device_id == enroll.device_id)
                    .expect("issued device on disk");
                fps.push(rec.cert_sha256);
            }
            assert_eq!(fps.len(), 3);
            assert_ne!(fps[0], fps[1]);
            assert_ne!(fps[1], fps[2]);
            let (tx, _rx) = mpsc::unbounded_channel();
            hub.hello(&params, tx, true, Some(fps[0].clone()))
                .expect("device 0");
            let (tx, _rx) = mpsc::unbounded_channel();
            hub.hello(&params, tx, true, Some(fps[1].clone()))
                .expect("device 1");
            let (tx, _rx) = mpsc::unbounded_channel();
            let err = hub
                .hello(&params, tx, true, Some(fps[2].clone()))
                .expect_err("device 2 over cap");
            assert_eq!(err.code, -32001);
            let (tx, mut rx) = mpsc::unbounded_channel();
            hub.hello(&params, tx, true, Some(fps[0].clone()))
                .expect("device 0 replace still allowed");
            assert!(rx.try_recv().is_err());
            assert_eq!(hub.lan_clients(), 2);
        });
    }

    #[test]
    fn hosted_tui_gets_phone_prompt_and_fans_out_updates() {
        let hub = Hub::new();
        let tui_hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "tui", "name": "gtm", "version": "0" },
        });
        let mac_hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "macos", "name": "app", "version": "0" },
        });
        let (tx_tui, mut rx_tui) = mpsc::unbounded_channel();
        let (tui, _) = hub.hello(&tui_hello, tx_tui, false, None).unwrap();
        match hub.handle_request(
            tui,
            json!(1),
            "gtm/sessions.host",
            json!({ "sessionId": "sess-1", "cwd": "/tmp", "title": "Live" }),
        ) {
            Dispatch::Reply(Ok(v)) => assert_eq!(v["ok"], true),
            Dispatch::Reply(Err(e)) => panic!("host: {e:?}"),
            Dispatch::Pending => panic!("host pending"),
        }
        let (tx_phone, mut rx_phone) = mpsc::unbounded_channel();
        let (phone, _) = hub.hello(&mac_hello, tx_phone, false, None).unwrap();
        match hub.handle_request(
            phone,
            json!(2),
            "session/resume",
            json!({ "sessionId": "sess-1", "cwd": "/tmp", "mcpServers": [] }),
        ) {
            Dispatch::Reply(Ok(v)) => assert_eq!(v["sessionId"], "sess-1"),
            Dispatch::Reply(Err(e)) => panic!("resume: {e:?}"),
            Dispatch::Pending => panic!("resume pending"),
        }
        match hub.handle_request(
            phone,
            json!(3),
            "session/prompt",
            json!({
                "sessionId": "sess-1",
                "prompt": [{ "type": "text", "text": "hello from phone" }]
            }),
        ) {
            Dispatch::Pending => {}
            Dispatch::Reply(Ok(v)) => panic!("prompt should be pending: {v}"),
            Dispatch::Reply(Err(e)) => panic!("prompt: {e:?}"),
        }
        let mut saw_user = false;
        while let Ok(msg) = rx_phone.try_recv() {
            if msg["method"] == "session/update" {
                saw_user = true;
                break;
            }
        }
        assert!(saw_user, "phone should see user_message_chunk");
        let mut prompt = None;
        while let Ok(msg) = rx_tui.try_recv() {
            if msg["method"] == "session/prompt" {
                prompt = Some(msg);
                break;
            }
        }
        let req = prompt.expect("tui should receive session/prompt");
        assert_eq!(req["id"], 3);
        hub.handle_notification(
            tui,
            "session/update",
            json!({
                "sessionId": "sess-1",
                "update": { "sessionUpdate": "agent_message_chunk", "text": "hi" }
            }),
        );
        let mut saw_agent = false;
        while let Ok(msg) = rx_phone.try_recv() {
            if msg["method"] == "session/update"
                && msg["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            {
                saw_agent = true;
                break;
            }
        }
        assert!(saw_agent, "phone should see TUI agent chunks");
        hub.handle_client_reply(
            tui,
            json!(3),
            Some(json!({ "stopReason": "end_turn" })),
            None,
        );
        let mut done = None;
        while let Ok(msg) = rx_phone.try_recv() {
            if msg.get("id") == Some(&json!(3)) {
                done = Some(msg);
                break;
            }
        }
        let done = done.expect("phone prompt result");
        assert_eq!(done["result"]["stopReason"], "end_turn");
    }

    #[test]
    fn tui_host_takes_over_existing_actor() {
        let hub = Hub::new();
        let mut actor_rx = hub.insert_live_for_test("sess-1");
        let tui_hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "tui", "name": "gtm", "version": "0" },
        });
        let mac_hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "macos", "name": "app", "version": "0" },
        });
        let (tx_tui, mut rx_tui) = mpsc::unbounded_channel();
        let (tui, _) = hub.hello(&tui_hello, tx_tui, false, None).unwrap();
        match hub.handle_request(
            tui,
            json!(1),
            "gtm/sessions.host",
            json!({ "sessionId": "sess-1", "cwd": "/tmp" }),
        ) {
            Dispatch::Reply(Ok(v)) => {
                assert_eq!(v["ok"], true);
                assert_eq!(v["tookOver"], true);
            }
            Dispatch::Reply(Err(e)) => panic!("host: {e:?}"),
            Dispatch::Pending => panic!("host pending"),
        }
        assert!(hub.is_hosted("sess-1"));
        assert!(matches!(actor_rx.try_recv(), Ok(ActorCmd::Shutdown)));
        let (tx_phone, _rx_phone) = mpsc::unbounded_channel();
        let (phone, _) = hub.hello(&mac_hello, tx_phone, false, None).unwrap();
        match hub.handle_request(
            phone,
            json!(2),
            "session/resume",
            json!({ "sessionId": "sess-1", "cwd": "/tmp", "mcpServers": [] }),
        ) {
            Dispatch::Reply(Ok(v)) => assert_eq!(v["hosted"], true),
            Dispatch::Reply(Err(e)) => panic!("resume: {e:?}"),
            Dispatch::Pending => panic!("resume pending"),
        }
        match hub.handle_request(
            phone,
            json!(3),
            "session/prompt",
            json!({
                "sessionId": "sess-1",
                "prompt": [{ "type": "text", "text": "hi" }]
            }),
        ) {
            Dispatch::Pending => {}
            Dispatch::Reply(Ok(v)) => panic!("expected pending {v}"),
            Dispatch::Reply(Err(e)) => panic!("prompt {e:?}"),
        }
        let mut saw = false;
        while let Ok(msg) = rx_tui.try_recv() {
            if msg["method"] == "session/prompt" {
                saw = true;
                break;
            }
        }
        assert!(saw, "TUI host must receive the phone prompt after takeover");
    }

    #[test]
    fn second_tui_host_replaces_the_first() {
        let hub = Hub::new();
        let hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "tui", "name": "gtm", "version": "0" },
        });
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (a, _) = hub.hello(&hello, tx1, false, None).unwrap();
        match hub.handle_request(
            a,
            json!(1),
            "gtm/sessions.host",
            json!({ "sessionId": "sess-1", "cwd": "/tmp" }),
        ) {
            Dispatch::Reply(Ok(v)) => assert_eq!(v["ok"], true),
            Dispatch::Reply(Err(e)) => panic!("first host: {e:?}"),
            Dispatch::Pending => panic!("first host pending"),
        }
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let (b, _) = hub.hello(&hello, tx2, false, None).unwrap();
        match hub.handle_request(
            b,
            json!(1),
            "gtm/sessions.host",
            json!({ "sessionId": "sess-1", "cwd": "/tmp" }),
        ) {
            Dispatch::Reply(Ok(v)) => assert_eq!(v["ok"], true),
            Dispatch::Reply(Err(e)) => panic!("replace must succeed: {e:?}"),
            Dispatch::Pending => panic!("pending"),
        }
        let (tx_phone, _rx_phone) = mpsc::unbounded_channel();
        let mac = json!({
            "protocolVersion": 1,
            "client": { "kind": "macos", "name": "app", "version": "0" },
        });
        let (phone, _) = hub.hello(&mac, tx_phone, false, None).unwrap();
        let _ = hub.handle_request(
            phone,
            json!(2),
            "session/resume",
            json!({ "sessionId": "sess-1", "cwd": "/tmp", "mcpServers": [] }),
        );
        match hub.handle_request(
            phone,
            json!(3),
            "session/prompt",
            json!({
                "sessionId": "sess-1",
                "prompt": [{ "type": "text", "text": "after resume" }]
            }),
        ) {
            Dispatch::Pending => {}
            Dispatch::Reply(Ok(v)) => panic!("expected pending {v}"),
            Dispatch::Reply(Err(e)) => panic!("prompt {e:?}"),
        }
        let mut saw = false;
        while let Ok(msg) = rx2.try_recv() {
            if msg["method"] == "session/prompt" {
                saw = true;
                break;
            }
        }
        assert!(saw, "new TUI must receive prompts after replacing the old host");
    }

    #[test]
    fn complete_session_does_not_overwrite_tui_host() {
        let hub = Hub::new();
        let tui_hello = json!({
            "protocolVersion": 1,
            "client": { "kind": "tui", "name": "gtm", "version": "0" },
        });
        let (tx_tui, _rx_tui) = mpsc::unbounded_channel();
        let (tui, _) = hub.hello(&tui_hello, tx_tui, false, None).unwrap();
        let _ = hub.handle_request(
            tui,
            json!(1),
            "gtm/sessions.host",
            json!({ "sessionId": "sess-1", "cwd": "/tmp" }),
        );
        let (atx, mut arx) = mpsc::unbounded_channel();
        hub.complete_session(
            "sess-1",
            Some("/tmp".into()),
            "boot".into(),
            json!({ "sessionId": "sess-1" }),
            ActorHandle { tx: atx },
            None,
        );
        assert!(hub.is_hosted("sess-1"));
        assert!(matches!(arx.try_recv(), Ok(ActorCmd::Shutdown)));
    }
}
