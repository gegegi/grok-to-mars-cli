//! Client helpers: connect, hello, spawn hub if missing.

use super::frame::{read_frame, write_frame};
use super::paths;
use super::server::PROTOCOL_VERSION;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::io::BufReader;

#[cfg(unix)]
pub struct HubClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: i64,
    pub inbox: VecDeque<Value>,
}

#[cfg(not(unix))]
pub struct HubClient {
    next_id: i64,
}

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
impl HubClient {
    pub async fn hello(&mut self, kind: &str, name: &str, version: &str) -> Result<Value> {
        self.request(
            "gtm/hello",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "client": { "kind": kind, "name": name, "version": version },
                "capabilities": { "permissionUi": true, "yolo": false },
            }),
        )
        .await
    }

    pub async fn hub_info(&mut self) -> Result<Value> {
        self.request("gtm/hub.info", json!({})).await
    }

    pub async fn sessions_list(&mut self) -> Result<Value> {
        self.request("gtm/sessions.list", json!({})).await
    }

    pub async fn ping(&mut self) -> Result<Value> {
        self.request("gtm/ping", json!({})).await
    }

    pub async fn remote_on(&mut self, port: Option<u16>) -> Result<Value> {
        let mut params = json!({});
        if let Some(port) = port {
            params["port"] = json!(port);
        }
        self.request("gtm/remote.on", params).await
    }

    pub async fn remote_off(&mut self) -> Result<Value> {
        self.request("gtm/remote.off", json!({})).await
    }

    pub async fn remote_status(&mut self) -> Result<Value> {
        self.request("gtm/remote.status", json!({})).await
    }

    pub async fn remote_revoke(&mut self, device_id: &str) -> Result<Value> {
        self.request("gtm/remote.revoke", json!({ "deviceId": device_id }))
            .await
    }

    pub async fn bye(&mut self) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": "gtm/bye" });
        write_frame(&mut self.writer, &serde_json::to_vec(&msg)?).await?;
        Ok(())
    }

    pub async fn initialize(&mut self) -> Result<Value> {
        self.request("initialize", json!({})).await
    }

    pub async fn session_new(&mut self, cwd: &str) -> Result<Value> {
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await
    }

    pub async fn session_load(&mut self, session_id: &str, cwd: &str) -> Result<Value> {
        self.request(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .await
    }

    pub async fn session_prompt(&mut self, session_id: &str, text: &str) -> Result<Value> {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": text }]
            }),
        )
        .await
    }

    pub async fn subscribe(&mut self, session_id: &str) -> Result<Value> {
        self.request("gtm/sessions.subscribe", json!({ "sessionId": session_id }))
            .await
    }

    pub async fn session_history(&mut self, session_id: &str, limit: u64) -> Result<Value> {
        self.request(
            "gtm/session.history",
            json!({ "sessionId": session_id, "limit": limit }),
        )
        .await
    }

    pub async fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        write_frame(&mut self.writer, &serde_json::to_vec(&msg)?).await?;
        Ok(id)
    }

    pub async fn wait_id_message(&mut self, id: &Value) -> Result<Value> {
        if let Some(i) = self.inbox.iter().position(|m| m.get("id") == Some(id)) {
            return Ok(self.inbox.remove(i).unwrap());
        }
        loop {
            let msg = self.read_socket().await?;
            if msg.get("id") == Some(id) {
                return Ok(msg);
            }
            self.inbox.push_back(msg);
        }
    }

    pub async fn wait_result(&mut self, id: i64) -> Result<Value> {
        loop {
            let reply = self.read_socket().await?;
            if reply.get("id") == Some(&json!(id)) {
                if let Some(err) = reply.get("error") {
                    bail!(
                        "hub error {}: {}",
                        err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1),
                        err.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                }
                return Ok(reply.get("result").cloned().unwrap_or(json!({})));
            }
            self.inbox.push_back(reply);
        }
    }

    pub async fn send_response(&mut self, id: Value, result: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        write_frame(&mut self.writer, &serde_json::to_vec(&msg)?).await?;
        Ok(())
    }

    pub async fn wait_method(&mut self, method: &str) -> Result<Value> {
        if let Some(i) = self
            .inbox
            .iter()
            .position(|m| m.get("method").and_then(|v| v.as_str()) == Some(method))
        {
            return Ok(self.inbox.remove(i).unwrap());
        }
        loop {
            let msg = self.read_socket().await?;
            if msg.get("method").and_then(|v| v.as_str()) == Some(method) {
                return Ok(msg);
            }
            self.inbox.push_back(msg);
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        write_frame(&mut self.writer, &serde_json::to_vec(&msg)?).await?;
        loop {
            let reply = self.read_socket().await?;
            if reply.get("id") == Some(&json!(id)) {
                if let Some(err) = reply.get("error") {
                    bail!(
                        "hub error {}: {}",
                        err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1),
                        err.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                }
                return Ok(reply.get("result").cloned().unwrap_or(json!({})));
            }
            self.inbox.push_back(reply);
        }
    }

    async fn read_socket(&mut self) -> Result<Value> {
        let raw = read_frame(&mut self.reader).await?;
        Ok(serde_json::from_slice(&raw)?)
    }

    pub async fn recv(&mut self) -> Result<Value> {
        if let Some(msg) = self.inbox.pop_front() {
            return Ok(msg);
        }
        self.read_socket().await
    }

    pub async fn send_raw(&mut self, msg: Value) -> Result<()> {
        write_frame(&mut self.writer, &serde_json::to_vec(&msg)?).await?;
        Ok(())
    }
}

#[cfg(unix)]
pub async fn connect() -> Result<HubClient> {
    connect_path(&paths::socket_path()).await
}

#[cfg(unix)]
pub async fn connect_path(path: &std::path::Path) -> Result<HubClient> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect {}", path.display()))?;
    let (reader, writer) = stream.into_split();
    Ok(HubClient {
        reader: BufReader::new(reader),
        writer,
        next_id: 1,
        inbox: VecDeque::new(),
    })
}

#[cfg(not(unix))]
pub async fn connect() -> Result<HubClient> {
    bail!("gtm hub is only supported on macOS and Linux")
}

/// Connect to the hub, spawning `gtm hub --daemon` if the socket is down.
pub async fn connect_or_spawn() -> Result<HubClient> {
    if let Ok(c) = connect().await {
        return Ok(c);
    }
    let sock = paths::socket_path();
    if paths::is_stale_socket(&sock) {
        paths::cleanup_stale();
    }
    spawn_hub()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(c) = connect().await {
            return Ok(c);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for gtm hub at {}", sock.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_hub() -> Result<u32> {
    let exe = std::env::current_exe().context("current_exe for gtm hub spawn")?;
    let mut cmd = Command::new(exe);
    cmd.arg("hub").arg("--daemon");
    if let Some(sock) = std::env::var_os(paths::SOCKET_ENV) {
        cmd.env(paths::SOCKET_ENV, sock);
    }
    if let Some(home) = std::env::var_os(paths::HOME_ENV) {
        cmd.env(paths::HOME_ENV, home);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    let log = paths::log_path();
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        Ok(file) => {
            cmd.stderr(std::process::Stdio::from(file));
        }
        Err(_) => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[allow(clippy::disallowed_methods)]
    let child = cmd.spawn().context("spawn gtm hub")?;
    Ok(child.id())
}
