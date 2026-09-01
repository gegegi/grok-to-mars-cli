#![cfg(unix)]

use gtm_hub::{AgentSpawn, Hub, connect_path, run_on_with};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fake_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_agent"))
}

fn temp_sock() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "gtm-hub-actor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("hub.sock");
    (dir, sock)
}

async fn start_hub() -> (tokio::task::JoinHandle<()>, PathBuf, PathBuf) {
    let (dir, sock) = temp_sock();
    let hub = Hub::with_agent(AgentSpawn {
        program: fake_bin(),
        args: vec![],
    });
    let server = tokio::spawn({
        let sock = sock.clone();
        async move {
            let _ = run_on_with(sock, hub).await;
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if connect_path(&sock).await.is_ok() {
            break;
        }
        if Instant::now() > deadline {
            server.abort();
            panic!("hub did not bind {}", sock.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (server, dir, sock)
}

async fn hello(sock: &std::path::Path) -> gtm_hub::HubClient {
    let mut c = connect_path(sock).await.expect("connect");
    c.hello("macos", "test", "0.0.0").await.expect("hello");
    c.initialize().await.expect("initialize");
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn session_new_prompt_and_list_live() {
    let (server, dir, sock) = start_hub().await;
    let mut a = hello(&sock).await;
    let created = a.session_new("/tmp/proj").await.expect("new");
    let sid = created["sessionId"].as_str().expect("sid").to_string();
    let prompted = a.session_prompt(&sid, "hi").await.expect("prompt");
    assert_eq!(prompted["stopReason"], "end_turn");
    let update = a.wait_method("session/update").await.expect("update");
    assert_eq!(update["params"]["sessionId"], sid);
    let list = a.sessions_list().await.expect("list");
    let sessions = list["sessions"].as_array().cloned().unwrap_or_default();
    assert!(
        sessions.iter().any(|s| s["id"] == sid && s["live"] == true),
        "live session missing: {sessions:?}"
    );
    let _ = a.bye().await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_client_gets_fanout() {
    let (server, dir, sock) = start_hub().await;
    let mut a = hello(&sock).await;
    let created = a.session_new("/tmp/proj").await.expect("new");
    let sid = created["sessionId"].as_str().unwrap().to_string();

    let mut b = hello(&sock).await;
    let sub = b.subscribe(&sid).await.expect("sub");
    assert_eq!(sub["live"], true);

    let prompted = a.session_prompt(&sid, "hi").await.expect("prompt");
    assert_eq!(prompted["stopReason"], "end_turn");
    let update = tokio::time::timeout(Duration::from_secs(2), b.wait_method("session/update"))
        .await
        .expect("timeout")
        .expect("update");
    assert_eq!(update["params"]["sessionId"], sid);

    let loaded = b.session_load(&sid, "/tmp/proj").await.expect("load live");
    assert_eq!(loaded["sessionId"], sid);

    let _ = a.bye().await;
    let _ = b.bye().await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_without_session_errors() {
    let (server, dir, sock) = start_hub().await;
    let mut a = hello(&sock).await;
    let err = a.session_prompt("missing", "hi").await;
    assert!(err.unwrap_err().to_string().contains("-32012"));
    let _ = a.bye().await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_first_reply_wins() {
    let (server, dir, sock) = start_hub().await;
    let mut a = hello(&sock).await;
    let created = a.session_new("/tmp/proj").await.expect("new");
    let sid = created["sessionId"].as_str().unwrap().to_string();
    let mut b = hello(&sock).await;
    b.subscribe(&sid).await.expect("sub");

    let prompt_id = a
        .send_request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": "NEED_PERM" }]
            }),
        )
        .await
        .unwrap();

    let perm_a = tokio::time::timeout(
        Duration::from_secs(2),
        a.wait_method("session/request_permission"),
    )
    .await
    .expect("a perm timeout")
    .expect("a perm");
    let perm_b = tokio::time::timeout(
        Duration::from_secs(2),
        b.wait_method("session/request_permission"),
    )
    .await
    .expect("b perm timeout")
    .expect("b perm");

    let id_a = perm_a.get("id").cloned().unwrap();
    let id_b = perm_b.get("id").cloned().unwrap();
    a.send_response(
        id_a,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }),
    )
    .await
    .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), a.wait_result(prompt_id))
        .await
        .expect("prompt timeout")
        .expect("prompt result");
    assert_eq!(result["stopReason"], "end_turn");

    let resolved = tokio::time::timeout(Duration::from_secs(2), b.wait_id_message(&id_b))
        .await
        .expect("b resolved timeout")
        .expect("b resolved");
    assert_eq!(resolved["error"]["code"], -32013);
    let note = tokio::time::timeout(
        Duration::from_secs(2),
        b.wait_method("gtm/permission.resolved"),
    )
    .await
    .expect("resolved note timeout")
    .expect("resolved note");
    assert_eq!(note["params"]["sessionId"], sid);

    let _ = a.bye().await;
    let _ = b.bye().await;
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
