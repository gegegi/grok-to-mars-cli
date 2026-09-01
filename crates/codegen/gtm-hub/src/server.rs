//! Unix listener and JSON-RPC dispatch.

use super::frame::{read_frame, write_frame};
use super::hub::{Dispatch, Hub};
use super::paths;
use super::rpc::rpc_error;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::signal;
use tokio::sync::mpsc;

pub const PROTOCOL_VERSION: u64 = 1;

#[cfg(unix)]
pub async fn run_foreground() -> Result<()> {
    paths::ensure_home()?;
    run_on(paths::socket_path()).await
}

#[cfg(unix)]
pub async fn run_on(sock: std::path::PathBuf) -> Result<()> {
    run_on_with(sock, Hub::new()).await
}

#[cfg(unix)]
pub async fn run_on_with(sock: std::path::PathBuf, hub: Hub) -> Result<()> {
    if paths::is_stale_socket(&sock) {
        let _ = std::fs::remove_file(&sock);
    }
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let lock_path = sock
        .parent()
        .map(|p| p.join("hub.lock"))
        .unwrap_or_else(|| sock.clone());
    let mut lock = acquire_lock(&lock_path)?;
    if sock.exists() {
        bail!("gtm hub already running (socket {})", sock.display());
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    }
    let pid = std::process::id();
    {
        use std::io::Write;
        lock.set_len(0)?;
        writeln!(lock, "{pid}")?;
        lock.flush()?;
    }
    println!("gtm hub listening on {} (pid {pid})", sock.display());

    let bind_path = sock.clone();
    let hub_accept = hub.clone();
    let server = async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let hub = hub_accept.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_unix(stream, hub).await {
                            tracing::debug!(error = %err, "hub client closed");
                        }
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "hub accept failed");
                }
            }
        }
    };
    let lan_loop = lan_supervisor(hub.clone());

    tokio::select! {
        _ = server => {}
        _ = lan_loop => {}
        _ = signal::ctrl_c() => {}
        _ = sigterm() => {}
    }

    hub.shutdown();
    let _ = std::fs::remove_file(&bind_path);
    if let Some(dir) = bind_path.parent() {
        let _ = std::fs::remove_file(dir.join("hub.lock"));
    }
    Ok(())
}

#[cfg(unix)]
async fn sigterm() {
    if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
        s.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn lan_supervisor(hub: Hub) {
    let mut rx = hub.subscribe_lan();
    loop {
        let want = rx.borrow().clone();
        if want.enabled {
            tokio::select! {
                res = run_lan_listener(hub.clone(), want.port) => {
                    if let Err(err) = res {
                        tracing::warn!(error = %err, "lan listener stopped");
                    }
                    let _ = tokio::time::sleep(Duration::from_millis(200)).await;
                    if !hub.lan_want().enabled {
                        continue;
                    }
                }
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        } else if rx.changed().await.is_err() {
            break;
        }
    }
}

#[cfg(unix)]
async fn run_lan_listener(hub: Hub, port: u16) -> Result<()> {
    let cfg = crate::tls::rustls_server_config()?;
    let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
    let bind = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind LAN {bind}"))?;
    tracing::info!("gtm hub LAN TLS on {bind}");
    println!("gtm hub LAN on {bind} (ALPN {})", crate::tls::ALPN);
    loop {
        let (tcp, addr) = listener.accept().await?;
        let _ = tcp.set_nodelay(true);
        tune_tcp_keepalive(&tcp);
        let hub = hub.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_lan(acceptor, tcp, hub).await {
                tracing::debug!(peer = %addr, error = %err, "lan client closed");
            }
        });
    }
}

#[cfg(unix)]
async fn handle_lan(
    acceptor: tokio_rustls::TlsAcceptor,
    tcp: tokio::net::TcpStream,
    hub: Hub,
) -> Result<()> {
    let tls = acceptor.accept(tcp).await.context("tls handshake")?;
    let fp = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|c| crate::tls::sha256_hex(c.as_ref()));
    let (reader, writer) = tokio::io::split(tls);
    handle_rpc(BufReader::new(reader), writer, hub, true, fp).await
}

#[cfg(unix)]
async fn handle_unix(stream: UnixStream, hub: Hub) -> Result<()> {
    if !peer_uid_ok(&stream) {
        bail!("hub rejected client: uid mismatch");
    }
    let (reader, writer) = stream.into_split();
    handle_rpc(BufReader::new(reader), writer, hub, false, None).await
}

#[cfg(unix)]
async fn handle_rpc<R, W>(
    mut reader: BufReader<R>,
    mut writer: W,
    hub: Hub,
    lan: bool,
    device_fp: Option<String>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let mut hello_done = false;
    let mut client_id: Option<u64> = None;
    let mut last_activity = Instant::now();
    let idle = Duration::from_secs(90);

    loop {
        let idle_left = idle.saturating_sub(last_activity.elapsed());
        tokio::select! {
            raw = read_frame(&mut reader) => {
                last_activity = Instant::now();
                let raw = raw?;
                let msg: Value = serde_json::from_slice(&raw)?;
                let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let id = msg.get("id").cloned();
                if !hello_done {
                    if method != "gtm/hello" {
                        if let Some(id) = id {
                            let err = rpc_error(id, -32000, "first request must be gtm/hello");
                            write_frame(&mut writer, &serde_json::to_vec(&err)?).await?;
                        }
                        bail!("first request must be gtm/hello");
                    }
                    let params = msg.get("params").cloned().unwrap_or(json!({}));
                    match hub.hello(&params, tx.clone(), lan, device_fp.clone()) {
                        Ok((cid, result)) => {
                            client_id = Some(cid);
                            hello_done = true;
                            if let Some(id) = id {
                                write_frame(
                                    &mut writer,
                                    &serde_json::to_vec(&json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": result,
                                    }))?,
                                )
                                .await?;
                            }
                        }
                        Err(err) => {
                            if let Some(id) = id {
                                write_frame(&mut writer, &serde_json::to_vec(&err.to_msg(id))?)
                                    .await?;
                            }
                            bail!("gtm/hello rejected");
                        }
                    }
                    continue;
                }
                if method == "gtm/bye" {
                    break;
                }
                let Some(cid) = client_id else { break };
                if method.is_empty() {
                    if let Some(id) = id {
                        hub.handle_client_reply(
                            cid,
                            id,
                            msg.get("result").cloned(),
                            msg.get("error").cloned(),
                        );
                    }
                    continue;
                }
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                if id.is_none() {
                    hub.handle_notification(cid, method, params);
                    continue;
                }
                let id = id.unwrap();
                match hub.handle_request(cid, id.clone(), method, params) {
                    Dispatch::Pending => {}
                    Dispatch::Reply(Ok(value)) => {
                        let _ = tx.send(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": value,
                        }));
                    }
                    Dispatch::Reply(Err(err)) => {
                        let _ = tx.send(err.to_msg(id));
                    }
                }
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                last_activity = Instant::now();
                write_frame(&mut writer, &serde_json::to_vec(&msg)?).await?;
                if msg.get("method").and_then(|v| v.as_str()) == Some("gtm/hub.shutdown") {
                    break;
                }
            }
            _ = tokio::time::sleep(idle_left) => {
                if last_activity.elapsed() >= idle {
                    break;
                }
            }
        }
    }
    if let Some(id) = client_id {
        hub.disconnect(id);
    }
    Ok(())
}

#[cfg(unix)]
fn tune_tcp_keepalive(tcp: &tokio::net::TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = tcp.as_raw_fd();
    unsafe {
        let on: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of_val(&on) as libc::socklen_t,
        );
        // Darwin uses TCP_KEEPALIVE for idle; Linux uses TCP_KEEPIDLE.
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "linux"
        ))]
        {
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            let idle_opt = libc::TCP_KEEPALIVE;
            #[cfg(target_os = "linux")]
            let idle_opt = libc::TCP_KEEPIDLE;
            let idle: libc::c_int = 15;
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                idle_opt,
                &idle as *const _ as *const libc::c_void,
                std::mem::size_of_val(&idle) as libc::socklen_t,
            );
            let intvl: libc::c_int = 5;
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPINTVL,
                &intvl as *const _ as *const libc::c_void,
                std::mem::size_of_val(&intvl) as libc::socklen_t,
            );
            let cnt: libc::c_int = 3;
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPCNT,
                &cnt as *const _ as *const libc::c_void,
                std::mem::size_of_val(&cnt) as libc::socklen_t,
            );
        }
    }
}

#[cfg(unix)]
fn acquire_lock(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("gtm hub already running");
    }
    Ok(file)
}

#[cfg(unix)]
fn peer_uid_ok(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        return rc == 0 && uid == unsafe { libc::geteuid() };
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        return rc == 0 && cred.uid == unsafe { libc::geteuid() };
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "linux"
    )))]
    {
        let _ = stream;
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::connect_path;

    #[tokio::test]
    async fn hello_then_info() {
        let dir = std::env::temp_dir().join(format!(
            "gtm-hub-it-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hub.sock");
        let server = tokio::spawn(run_on(sock.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut client = loop {
            if let Ok(c) = connect_path(&sock).await {
                break c;
            }
            if std::time::Instant::now() > deadline {
                server.abort();
                panic!("hub did not bind {}", sock.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let hello = client.hello("macos", "test", "0.0.0").await.expect("hello");
        assert_eq!(hello["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(hello["transport"], "unix");
        let info = client.hub_info().await.expect("info");
        assert_eq!(info["running"], true);
        let list = client.sessions_list().await.expect("list");
        assert!(list.get("sessions").and_then(|v| v.as_array()).is_some());
        let _ = client.bye().await;
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_bad_first_method() {
        use crate::{read_frame, write_frame};
        let dir = std::env::temp_dir().join(format!(
            "gtm-hub-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hub.sock");
        let server = tokio::spawn(run_on(sock.clone()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let stream = loop {
            if let Ok(s) = tokio::net::UnixStream::connect(&sock).await {
                break s;
            }
            if std::time::Instant::now() > deadline {
                server.abort();
                panic!("no bind");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let (reader, mut writer) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let msg = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"gtm/hub.info","params":{}});
        write_frame(&mut writer, &serde_json::to_vec(&msg).unwrap())
            .await
            .unwrap();
        let raw = read_frame(&mut reader).await.unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(reply["error"]["code"], -32000);
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lan_mtls_ios_hello_session_new_denied() {
        use crate::tls;
        use crate::{read_frame, write_frame};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
        use std::sync::Arc;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let _home = crate::paths::lock_test_home();
        let dir = std::env::temp_dir().join(format!(
            "gtm-hub-lan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("GTM_HOME", &dir) };
        let sock = dir.join("hub.sock");
        let hub = Hub::new();
        let server = tokio::spawn(run_on_with(sock.clone(), hub));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut local = loop {
            if let Ok(c) = crate::connect_path(&sock).await {
                break c;
            }
            if std::time::Instant::now() > deadline {
                server.abort();
                unsafe { std::env::remove_var("GTM_HOME") };
                panic!("no unix bind");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        local.hello("macos", "test", "0.0.0").await.unwrap();
        let enroll = tls::issue_device(86_400).unwrap();
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let on = local.remote_on(Some(port)).await.expect("remote.on");
        assert_eq!(on["lan"], true);

        let tcp_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let tcp = loop {
            if let Ok(s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                break s;
            }
            if std::time::Instant::now() > tcp_deadline {
                server.abort();
                unsafe { std::env::remove_var("GTM_HOME") };
                panic!("LAN did not bind");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(enroll.ca_cert_pem.as_bytes()).unwrap())
            .unwrap();
        let cert = CertificateDer::from_pem_slice(enroll.device_cert_pem.as_bytes()).unwrap();
        let key = PrivateKeyDer::from_pem_slice(enroll.device_key_pem.as_bytes()).unwrap();
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)
            .unwrap();
        cfg.alpn_protocols = vec![tls::ALPN.as_bytes().to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let name = ServerName::try_from("gtm-hub").unwrap();
        let tls_stream = connector.connect(name, tcp).await.expect("mTLS");
        let (reader, mut writer) = tokio::io::split(tls_stream);
        let mut reader = tokio::io::BufReader::new(reader);
        let hello = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "gtm/hello",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "client": { "kind": "ios", "name": "phone", "version": "0" },
                "capabilities": { "permissionUi": true, "yolo": true }
            }
        });
        write_frame(&mut writer, &serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        let raw = read_frame(&mut reader).await.unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(reply["result"]["transport"], "lan");
        assert_eq!(reply["result"]["kind"], "ios");

        let new_sess = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": "/tmp" }
        });
        write_frame(&mut writer, &serde_json::to_vec(&new_sess).unwrap())
            .await
            .unwrap();
        let raw = read_frame(&mut reader).await.unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(reply["error"]["code"], -32000);

        let _ = local.remote_off().await;
        let _ = local.bye().await;
        server.abort();
        unsafe { std::env::remove_var("GTM_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
