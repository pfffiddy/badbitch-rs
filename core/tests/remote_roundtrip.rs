//! End-to-end test of the remote stack: spawn a real controller with a mock
//! backend, enable the TLS WebSocket server, pair a client over localhost,
//! drive the app through the client, and watch state round-trip.

use llm_desk_core::backend::*;
use llm_desk_core::controller;
use llm_desk_core::protocol::Cmd;
use llm_desk_core::remote::client::{LinkStatus, RemoteSession};
use llm_desk_core::remote::pairing;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct MockBackend;
impl Backend for MockBackend {
    fn name(&self) -> &'static str { "mock" }
    fn ensure_running(&self) -> anyhow::Result<bool> { Ok(false) }
    fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo { name: "mock-7b".into(), ..Default::default() }])
    }
    fn chat_stream(&self, _req: &ChatRequest, on_token: TokenSink) -> anyhow::Result<String> {
        let reply = r#"{"thought":"","action":"final_answer","final_answer":"hello from mock"}"#;
        on_token(reply);
        Ok(reply.to_string())
    }
    fn pull_model(&self, _n: &str, _p: ProgressSink) -> anyhow::Result<()> { Ok(()) }
    fn import_gguf(&self, _g: &str, _m: &str, _p: ProgressSink) -> anyhow::Result<()> { Ok(()) }
}

fn wait_until(deadline_ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    while Instant::now() < deadline {
        if f() { return true; }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

#[test]
fn pair_auth_command_state_roundtrip() {
    // Isolate config dirs for server and client.
    let tmp = std::env::temp_dir().join(format!("llmdesk-test-{}", std::process::id()));
    let server_dir = tmp.join("server");
    let client_dir = tmp.join("client");
    std::fs::create_dir_all(&client_dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &server_dir); // tools::config_dir() → server_dir/llm-desk

    let handle = controller::spawn(Arc::new(MockBackend));

    // Wait for startup (model list from mock).
    assert!(wait_until(3000, || handle.shared.get().models.len() == 1), "controller startup");

    // Enable the server on a test port and start pairing.
    let port = 47321u16;
    handle.send(Cmd::SetRemotePort { port });
    handle.send(Cmd::SetRemoteEnabled { enabled: true });
    assert!(wait_until(3000, || handle.shared.get().remote.enabled), "server enabled");
    let fp_server = handle.shared.get().remote.fingerprint;
    assert_eq!(fp_server.len(), 64, "fingerprint published");

    handle.send(Cmd::StartPairing);
    assert!(wait_until(2000, || handle.shared.get().remote.pairing.is_some()), "pairing window");
    let code = handle.shared.get().remote.pairing.unwrap().code;
    assert_eq!(pairing::normalize_code(&code).len(), 16);

    // --- phone side ---
    let session = RemoteSession::new(client_dir.clone());
    session.connect_pair("127.0.0.1".into(), port, code, "test-phone".into());
    assert!(
        wait_until(5000, || session.link.lock().unwrap().status == LinkStatus::Connected),
        "pairing connect: {:?}",
        session.link.lock().unwrap().status
    );

    // Credentials saved, fingerprint pinned correctly.
    let saved = session.link.lock().unwrap().saved.clone().unwrap();
    assert_eq!(saved.fp_hex, fp_server);
    assert!(client_dir.join("server.json").exists());

    // State mirrored to the phone.
    assert!(wait_until(3000, || session.shared.get().models.len() == 1), "state mirrored");
    assert!(wait_until(2000, || session.shared.get().remote.connected_clients == 1));
    assert!(wait_until(2000, || session.shared.get().remote.devices.len() == 1), "device registered");

    // Drive the app from the phone: run a full agent turn on the desktop.
    session.send(Cmd::SendPrompt { text: "hi".into() });
    assert!(
        wait_until(5000, || {
            session.shared.get().transcript.iter().any(|t| {
                matches!(t, llm_desk_core::protocol::TranscriptItem::Assistant { text, .. } if text == "hello from mock")
            })
        }),
        "agent round-trip via remote: transcript = {:?}",
        session.shared.get().transcript
    );

    // --- upload a model file from the phone; the desktop imports it ---
    let gguf = tmp.join("tiny-upload.gguf");
    std::fs::write(&gguf, vec![0u8; 300 * 1024]).unwrap(); // spans several chunks
    session.upload_model_path(gguf.clone());
    assert!(
        wait_until(8000, || handle.shared.get().selected_model == "tiny-upload"),
        "uploaded model imported: status={:?} selected={:?} log={:?}",
        handle.shared.get().status,
        handle.shared.get().selected_model,
        handle.shared.get().ingest.log,
    );
    // The received bytes were written into the desktop's uploads dir.
    assert!(
        server_dir.join("llm-desk").join("uploads").join("tiny-upload.gguf").exists(),
        "uploaded file persisted on the desktop"
    );
    // Client-side progress was cleared once the transfer finished.
    assert!(
        wait_until(2000, || session.link.lock().unwrap().upload.is_none()),
        "upload progress cleared after completion"
    );

    // --- reconnect with the saved token (no code) ---
    session.disconnect();
    assert!(
        wait_until(3000, || handle.shared.get().remote.connected_clients == 0),
        "disconnect observed"
    );
    let session2 = RemoteSession::new(client_dir.clone());
    session2.connect_saved();
    assert!(
        wait_until(5000, || session2.link.lock().unwrap().status == LinkStatus::Connected),
        "token reconnect: {:?}",
        session2.link.lock().unwrap().status
    );

    // --- revoke kills future auth ---
    let dev_id = handle.shared.get().remote.devices[0].id.clone();
    handle.send(Cmd::RevokeDevice { id: dev_id });
    assert!(wait_until(2000, || handle.shared.get().remote.devices.is_empty()));
    session2.disconnect();
    let session3 = RemoteSession::new(client_dir.clone());
    session3.connect_saved();
    assert!(
        wait_until(5000, || matches!(session3.link.lock().unwrap().status, LinkStatus::Error(_))),
        "revoked token must be refused"
    );

    // --- wrong pairing code is rejected and closes the window ---
    handle.send(Cmd::StartPairing);
    assert!(wait_until(2000, || handle.shared.get().remote.pairing.is_some()));
    let session4 = RemoteSession::new(tmp.join("client2"));
    session4.connect_pair("127.0.0.1".into(), port, "WRONG-CODE-2345-6789".into(), "evil".into());
    assert!(wait_until(5000, || matches!(session4.link.lock().unwrap().status, LinkStatus::Error(_))));
    assert!(wait_until(2000, || handle.shared.get().remote.pairing.is_none()), "one strike closes window");
    assert_eq!(handle.shared.get().remote.devices.len(), 0);

    let _ = std::fs::remove_dir_all(&tmp);
}
