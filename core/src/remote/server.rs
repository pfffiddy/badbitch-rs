//! The desktop's remote-access server: rustls + tungstenite over plain
//! blocking std sockets (matching the rest of the app — no async runtime).
//!
//! Each connection must open with either `Pair` (during an active pairing
//! window) or `Auth` (a previously issued device token). Only then does it
//! get the state stream and the right to send commands. Unauthenticated
//! sockets are answered with one error frame and closed.

use crate::protocol::{ClientMsg, Cmd, ServerMsg, SharedState};
use crate::remote::pairing::{self, RemoteStore};
use crate::remote::tls;
use anyhow::{Context, Result};
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ServerCtl {
    run: Arc<AtomicBool>,
    pub port: u16,
    pub fingerprint_hex: String,
}

impl ServerCtl {
    pub fn stop(&self) {
        self.run.store(false, Ordering::Relaxed);
    }
}

pub fn start(
    port: u16,
    config_dir: &Path,
    store: Arc<Mutex<RemoteStore>>,
    clients: Arc<AtomicUsize>,
    cmds: Sender<Cmd>,
    shared: SharedState,
) -> Result<ServerCtl> {
    // Make sure the ring crypto provider is installed exactly once.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ident = tls::load_or_create(config_dir).context("TLS identity")?;
    let fingerprint_hex = tls::fp_hex(&ident.fingerprint);
    let fp = ident.fingerprint;

    let cert = rustls::pki_types::CertificateDer::from(ident.cert_der.clone());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(ident.key_pkcs8_der.clone().into());
    let tls_cfg = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .context("server TLS config")?,
    );

    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("bind 0.0.0.0:{port}"))?;
    listener.set_nonblocking(true).context("nonblocking listener")?;

    let run = Arc::new(AtomicBool::new(true));
    let run2 = run.clone();
    let dir: PathBuf = config_dir.to_path_buf();

    std::thread::spawn(move || {
        while run2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let tls_cfg = tls_cfg.clone();
                    let store = store.clone();
                    let clients = clients.clone();
                    let cmds = cmds.clone();
                    let shared = shared.clone();
                    let run = run2.clone();
                    let dir = dir.clone();
                    std::thread::spawn(move || {
                        let _ = handle_conn(stream, tls_cfg, store, clients, cmds, shared, run, dir, fp);
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(300)),
            }
        }
    });

    Ok(ServerCtl { run, port, fingerprint_hex })
}

type Ws = tungstenite::WebSocket<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>;

#[allow(clippy::too_many_arguments)]
fn handle_conn(
    stream: TcpStream,
    tls_cfg: Arc<rustls::ServerConfig>,
    store: Arc<Mutex<RemoteStore>>,
    clients: Arc<AtomicUsize>,
    cmds: Sender<Cmd>,
    shared: SharedState,
    run: Arc<AtomicBool>,
    config_dir: PathBuf,
    fp: [u8; 32],
) -> Result<()> {
    // Keep a handle to the raw socket so we can flip read timeouts after the
    // (blocking) TLS + WebSocket handshakes are done.
    let raw = stream.try_clone().context("clone tcp stream")?;

    let conn = rustls::ServerConnection::new(tls_cfg).context("tls conn")?;
    let tls_stream = rustls::StreamOwned::new(conn, stream);
    let mut ws: Ws = tungstenite::accept(tls_stream).context("ws accept")?;

    // --- authentication gate -------------------------------------------
    // Give the client 10 s and at most a handful of frames to present
    // credentials — no free-riding pre-auth.
    raw.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut budget = 8u8;
    let first = loop {
        budget = budget.saturating_sub(1);
        if budget == 0 {
            return Ok(());
        }
        match ws.read() {
            Ok(m) if m.is_text() => break m.into_text().unwrap_or_default(),
            Ok(m) if m.is_close() => return Ok(()),
            Ok(_) => continue, // ignore ping/pong/binary pre-auth
            Err(_) => return Ok(()),
        }
    };

    let authed = match serde_json::from_str::<ClientMsg>(&first) {
        Ok(ClientMsg::Pair { name, mac }) => {
            let name = name.chars().take(64).collect::<String>();
            match pairing::try_pair(&store, &config_dir, &fp, &name, &mac) {
                Ok(token) => {
                    send_msg(&mut ws, &ServerMsg::PairOk { token, fp: tls::fp_hex(&fp) })?;
                    true
                }
                Err(e) => {
                    let _ = send_msg(&mut ws, &ServerMsg::Err { msg: e });
                    false
                }
            }
        }
        Ok(ClientMsg::Auth { token }) => {
            if pairing::check_token(&store, &token) {
                send_msg(&mut ws, &ServerMsg::AuthOk)?;
                true
            } else {
                let _ = send_msg(&mut ws, &ServerMsg::Err { msg: "invalid token (revoked?)".into() });
                false
            }
        }
        _ => {
            let _ = send_msg(&mut ws, &ServerMsg::Err { msg: "authenticate first".into() });
            false
        }
    };
    if !authed {
        let _ = ws.close(None);
        return Ok(());
    }

    // --- serve ----------------------------------------------------------
    clients.fetch_add(1, Ordering::Relaxed);
    let result = serve(&mut ws, &raw, &cmds, &shared, &run, &config_dir);
    clients.fetch_sub(1, Ordering::Relaxed);
    result
}

/// A model file streaming in from a client, one binary frame at a time.
struct Upload {
    file: std::fs::File,
    path: PathBuf,
    received: u64,
    total: u64,
    last_report: u64,
}

impl Upload {
    /// Best-effort delete of the partial file (on cancel / disconnect).
    fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Keep only safe characters and force a `.gguf` extension so an uploaded
/// name can never escape the uploads directory or be mistaken for anything
/// else. Falls back to a fixed name if nothing usable is left.
fn safe_upload_name(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches(['.', '-']).to_string();
    let stem = cleaned.strip_suffix(".gguf").unwrap_or(&cleaned);
    let stem = if stem.is_empty() { "uploaded-model" } else { stem };
    format!("{stem}.gguf")
}

fn serve(
    ws: &mut Ws,
    raw: &TcpStream,
    cmds: &Sender<Cmd>,
    shared: &SharedState,
    run: &Arc<AtomicBool>,
    config_dir: &Path,
) -> Result<()> {
    use std::io::Write;

    // Cap an upload at 64 GiB — comfortably above any real GGUF, but a bound
    // so a misbehaving client can't fill the disk unchecked.
    const MAX_UPLOAD: u64 = 64 * 1024 * 1024 * 1024;

    // Short read timeout doubles as the loop pacing: we alternate between
    // "any incoming command?" and "any newer state to push?".
    raw.set_read_timeout(Some(Duration::from_millis(100))).ok();
    let mut last_sent: u64 = 0;
    let mut upload: Option<Upload> = None;

    while run.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(m) if m.is_text() => {
                match serde_json::from_str::<ClientMsg>(&m.into_text().unwrap_or_default()) {
                    Ok(ClientMsg::Cmd { cmd }) => {
                        let _ = cmds.send(cmd);
                    }
                    Ok(ClientMsg::UploadBegin { name, size }) => {
                        // Replace any half-finished prior upload on this socket.
                        if let Some(u) = upload.take() {
                            u.discard();
                        }
                        if size > MAX_UPLOAD {
                            continue; // refuse absurd sizes; leave `upload` unset
                        }
                        let dir = config_dir.join("uploads");
                        let _ = std::fs::create_dir_all(&dir);
                        let path = dir.join(safe_upload_name(&name));
                        match std::fs::File::create(&path) {
                            Ok(file) => {
                                let _ = cmds.send(Cmd::UploadStatus { received: 0, total: size });
                                upload = Some(Upload { file, path, received: 0, total: size, last_report: 0 });
                            }
                            Err(_) => { /* ignore; client will time out its progress */ }
                        }
                    }
                    Ok(ClientMsg::UploadEnd) => {
                        if let Some(mut u) = upload.take() {
                            let _ = u.file.flush();
                            drop(u.file);
                            let _ = cmds.send(Cmd::UploadStatus { received: u.received, total: u.total.max(u.received) });
                            // Hand the received file to the normal import pipeline.
                            let _ = cmds.send(Cmd::ImportPath { path: u.path.to_string_lossy().to_string() });
                        }
                    }
                    Ok(ClientMsg::UploadCancel) => {
                        if let Some(u) = upload.take() {
                            u.discard();
                        }
                    }
                    _ => {}
                }
            }
            Ok(m) if m.is_binary() => {
                if let Some(u) = upload.as_mut() {
                    let bytes = m.into_data();
                    // Enforce the size bound as bytes actually arrive.
                    if u.received.saturating_add(bytes.len() as u64) > MAX_UPLOAD {
                        if let Some(u) = upload.take() {
                            u.discard();
                        }
                        continue;
                    }
                    if u.file.write_all(&bytes).is_err() {
                        if let Some(u) = upload.take() {
                            u.discard();
                        }
                        continue;
                    }
                    u.received += bytes.len() as u64;
                    // Report roughly every 8 MiB so both GUIs show it moving.
                    if u.received - u.last_report >= 8 * 1024 * 1024 {
                        u.last_report = u.received;
                        let _ = cmds.send(Cmd::UploadStatus { received: u.received, total: u.total });
                    }
                }
            }
            Ok(m) if m.is_close() => break,
            Ok(_) => {} // ping/pong handled by tungstenite
            Err(tungstenite::Error::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(_) => break,
        }

        // Don't push state to a client that is mid-upload — it isn't reading,
        // so a large snapshot could back up the socket and stall the transfer.
        if upload.is_none() {
            let v = shared.version();
            if v > last_sent {
                let state = shared.get();
                last_sent = state.version;
                send_msg(ws, &ServerMsg::State { state })?;
            }
        }
    }
    if let Some(u) = upload.take() {
        u.discard();
    }
    let _ = ws.close(None);
    Ok(())
}

fn send_msg(ws: &mut Ws, msg: &ServerMsg) -> Result<()> {
    let json = serde_json::to_string(msg)?;
    ws.send(tungstenite::Message::Text(json)).context("ws send")?;
    Ok(())
}
