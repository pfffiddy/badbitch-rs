//! The phone's side of the remote protocol. A `RemoteSession` exposes the
//! same two things a local controller does — a `SharedState` to render and a
//! `Cmd` sender — so the UI code cannot tell (and doesn't care) whether it's
//! driving a local Ollama or a desktop across the network.

use crate::protocol::{ClientMsg, Cmd, Notify, ServerMsg, SharedState};
use crate::remote::pairing;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Something to send up the socket: either a control command or a model file
/// to stream (which needs binary frames, so it can't ride the `Cmd` path).
enum Outgoing {
    Cmd(Cmd),
    Upload(UploadReq),
}

/// A model file to push to the desktop, sourced either from a filesystem path
/// (desktop / a copy the Android layer already staged) or from bytes in memory.
pub struct UploadReq {
    pub name: String,
    pub source: UploadSource,
}

pub enum UploadSource {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

/// Client-side view of an upload in flight, for a progress bar on the phone.
#[derive(Debug, Clone)]
pub struct UploadProgress {
    pub sent: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    pub fp_hex: String,
    pub token_hex: String,
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkStatus {
    Idle,
    Connecting,
    Connected,
    Error(String),
}

pub struct Link {
    pub status: LinkStatus,
    pub saved: Option<ServerCfg>,
    /// Set while a model file is streaming up from this device.
    pub upload: Option<UploadProgress>,
}

pub struct RemoteSession {
    pub shared: SharedState,
    pub link: Arc<Mutex<Link>>,
    cmd_tx: Sender<Outgoing>,
    cmd_rx: Arc<Mutex<Receiver<Outgoing>>>,
    notifiers: Arc<Mutex<Vec<Notify>>>,
    config_dir: PathBuf,
    generation: Arc<Mutex<u64>>, // bumped on disconnect to retire old threads
}

impl RemoteSession {
    pub fn new(config_dir: PathBuf) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Outgoing>();
        let saved: Option<ServerCfg> = std::fs::read_to_string(config_dir.join("server.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        Arc::new(Self {
            shared: SharedState::new(),
            link: Arc::new(Mutex::new(Link { status: LinkStatus::Idle, saved, upload: None })),
            cmd_tx,
            cmd_rx: Arc::new(Mutex::new(cmd_rx)),
            notifiers: Arc::new(Mutex::new(Vec::new())),
            config_dir,
            generation: Arc::new(Mutex::new(0)),
        })
    }

    pub fn add_notifier(&self, n: Notify) {
        self.notifiers.lock().unwrap().push(n);
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(Outgoing::Cmd(cmd));
    }

    /// Queue a model file (already on disk) to stream up to the desktop, which
    /// will import it exactly as if it had been dropped on the desktop window.
    pub fn upload_model_path(&self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "model.gguf".into());
        let _ = self.cmd_tx.send(Outgoing::Upload(UploadReq {
            name,
            source: UploadSource::Path(path),
        }));
    }

    /// Queue in-memory model bytes (e.g. from a file the phone handed us) to
    /// stream up to the desktop for import.
    pub fn upload_model_bytes(&self, name: String, bytes: Vec<u8>) {
        let _ = self.cmd_tx.send(Outgoing::Upload(UploadReq {
            name,
            source: UploadSource::Bytes(bytes),
        }));
    }

    pub fn notify(&self) {
        for n in self.notifiers.lock().unwrap().iter() {
            n();
        }
    }

    pub fn forget_server(&self) {
        let _ = std::fs::remove_file(self.config_dir.join("server.json"));
        self.link.lock().unwrap().saved = None;
    }

    pub fn disconnect(self: &Arc<Self>) {
        *self.generation.lock().unwrap() += 1;
        let mut link = self.link.lock().unwrap();
        link.status = LinkStatus::Idle;
    }

    /// First-time connection with a pairing code shown on the desktop.
    pub fn connect_pair(self: &Arc<Self>, host: String, port: u16, code: String, device_name: String) {
        self.spawn_conn(host, port, Some((code, device_name)));
    }

    /// Reconnect with saved credentials.
    pub fn connect_saved(self: &Arc<Self>) {
        let saved = self.link.lock().unwrap().saved.clone();
        if let Some(cfg) = saved {
            self.spawn_conn(cfg.host.clone(), cfg.port, None);
        }
    }

    fn spawn_conn(self: &Arc<Self>, host: String, port: u16, pair: Option<(String, String)>) {
        let me = self.clone();
        let my_gen = {
            let mut g = self.generation.lock().unwrap();
            *g += 1;
            *g
        };
        {
            let mut link = self.link.lock().unwrap();
            link.status = LinkStatus::Connecting;
        }
        self.notify();
        std::thread::spawn(move || {
            let res = me.run_conn(&host, port, pair, my_gen);
            let mut link = me.link.lock().unwrap();
            if *me.generation.lock().unwrap() == my_gen {
                link.status = match res {
                    Ok(()) => LinkStatus::Idle, // clean close
                    Err(e) => LinkStatus::Error(format!("{e:#}")),
                };
                link.upload = None; // no transfer survives a dropped connection
            }
            drop(link);
            me.notify();
        });
    }

    fn run_conn(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        pair: Option<(String, String)>,
        my_gen: u64,
    ) -> Result<()> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Resolve + connect with a timeout so a wrong IP fails fast.
        let addr = (host, port)
            .to_socket_addrs()
            .context("resolve host")?
            .next()
            .ok_or_else(|| anyhow!("host resolved to no addresses"))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(6))
            .context("TCP connect (is the desktop server enabled and reachable?)")?;
        stream.set_nodelay(true).ok();
        let raw = stream.try_clone().context("clone stream")?;

        // Certificate policy: enforce the pinned fingerprint when we have
        // one; during pairing accept-and-capture (the HMAC binds the code to
        // whatever fingerprint we saw, so a MITM gains nothing).
        let pinned: Option<[u8; 32]> = if pair.is_some() {
            None
        } else {
            let saved = self.link.lock().unwrap().saved.clone();
            let cfg = saved.ok_or_else(|| anyhow!("no saved server — pair first"))?;
            let bytes = pairing::unhex(&cfg.fp_hex).ok_or_else(|| anyhow!("bad saved fingerprint"))?;
            Some(bytes.try_into().map_err(|_| anyhow!("bad saved fingerprint length"))?)
        };
        let seen_fp: Arc<Mutex<Option<[u8; 32]>>> = Arc::new(Mutex::new(None));
        let verifier = Arc::new(PinVerifier {
            pinned,
            seen: seen_fp.clone(),
            provider: rustls::crypto::ring::default_provider(),
        });
        let tls_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
            .unwrap_or_else(|_| rustls::pki_types::ServerName::try_from("llm-desk").unwrap());
        let conn = rustls::ClientConnection::new(Arc::new(tls_cfg), server_name)
            .context("tls client")?;
        let tls_stream = rustls::StreamOwned::new(conn, stream);
        let (mut ws, _resp) = tungstenite::client(format!("wss://{host}:{port}/"), tls_stream)
            .map_err(|e| anyhow!("TLS/WebSocket handshake failed: {e}"))?;

        // --- authenticate -------------------------------------------------
        let first = if let Some((code, device_name)) = &pair {
            let fp = seen_fp
                .lock()
                .unwrap()
                .ok_or_else(|| anyhow!("no server certificate seen"))?;
            ClientMsg::Pair {
                name: device_name.clone(),
                mac: pairing::hmac_code(code, &fp, device_name),
            }
        } else {
            let cfg = self.link.lock().unwrap().saved.clone().unwrap();
            ClientMsg::Auth { token: cfg.token_hex }
        };
        ws.send(tungstenite::Message::Text(serde_json::to_string(&first)?))
            .context("send auth")?;

        raw.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let reply: ServerMsg = loop {
            match ws.read() {
                Ok(m) if m.is_text() => {
                    break serde_json::from_str(&m.into_text().unwrap_or_default())
                        .context("bad server reply")?
                }
                Ok(m) if m.is_close() => return Err(anyhow!("server closed during auth")),
                Ok(_) => continue,
                Err(e) => return Err(anyhow!("read auth reply: {e}")),
            }
        };
        match reply {
            ServerMsg::PairOk { token, fp } => {
                let (_, device_name) = pair.as_ref().unwrap();
                let cfg = ServerCfg {
                    host: host.to_string(),
                    port,
                    fp_hex: fp,
                    token_hex: token,
                    device_name: device_name.clone(),
                };
                std::fs::create_dir_all(&self.config_dir).ok();
                let _ = std::fs::write(
                    self.config_dir.join("server.json"),
                    serde_json::to_string_pretty(&cfg)?,
                );
                self.link.lock().unwrap().saved = Some(cfg);
            }
            ServerMsg::AuthOk => {}
            ServerMsg::Err { msg } => return Err(anyhow!("server refused: {msg}")),
            _ => return Err(anyhow!("unexpected server reply")),
        }

        {
            let mut link = self.link.lock().unwrap();
            link.status = LinkStatus::Connected;
        }
        self.notify();

        // --- pump ----------------------------------------------------------
        raw.set_read_timeout(Some(Duration::from_millis(100))).ok();
        let mut last_ping = Instant::now();
        loop {
            if *self.generation.lock().unwrap() != my_gen {
                let _ = ws.close(None);
                return Ok(()); // superseded / user disconnect
            }
            match ws.read() {
                Ok(m) if m.is_text() => {
                    if let Ok(ServerMsg::State { state }) =
                        serde_json::from_str::<ServerMsg>(&m.into_text().unwrap_or_default())
                    {
                        // Preserve the server's version counter verbatim.
                        self.shared.publish_raw(state);
                        self.notify();
                    }
                }
                Ok(m) if m.is_close() => return Err(anyhow!("server closed the connection")),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(anyhow!("connection lost: {e}")),
            }

            // Forward queued commands and uploads.
            loop {
                let out = self.cmd_rx.lock().unwrap().try_recv();
                match out {
                    Ok(Outgoing::Cmd(cmd)) => {
                        let msg = serde_json::to_string(&ClientMsg::Cmd { cmd })?;
                        ws.send(tungstenite::Message::Text(msg)).context("send cmd")?;
                    }
                    Ok(Outgoing::Upload(req)) => {
                        // Streams the whole file; the server holds off on state
                        // frames meanwhile, so we won't miss anything.
                        self.stream_upload(&mut ws, req, my_gen)?;
                        last_ping = Instant::now();
                    }
                    Err(_) => break,
                }
            }

            // Keep NAT/VPN mappings alive.
            if last_ping.elapsed() > Duration::from_secs(20) {
                let _ = ws.send(tungstenite::Message::Ping(vec![]));
                last_ping = Instant::now();
            }
        }
    }

    /// Stream one model file to the desktop as `UploadBegin` + binary chunks +
    /// `UploadEnd`. Publishes progress to `link.upload` as it goes. Honors a
    /// generation bump (disconnect) by cancelling cleanly.
    fn stream_upload(self: &Arc<Self>, ws: &mut ClientWs, req: UploadReq, my_gen: u64) -> Result<()> {
        const CHUNK: usize = 256 * 1024;

        let (mut reader, total): (Box<dyn Read + Send>, u64) = match req.source {
            UploadSource::Path(ref p) => {
                let f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                (Box::new(f), len)
            }
            UploadSource::Bytes(v) => {
                let len = v.len() as u64;
                (Box::new(std::io::Cursor::new(v)), len)
            }
        };

        let set_progress = |sent: u64| {
            self.link.lock().unwrap().upload = Some(UploadProgress { sent, total });
            self.notify();
        };

        ws.send(tungstenite::Message::Text(serde_json::to_string(
            &ClientMsg::UploadBegin { name: req.name.clone(), size: total },
        )?))
        .context("send UploadBegin")?;
        set_progress(0);

        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        let mut last_notify = Instant::now();
        loop {
            // Disconnect / supersede: abort the transfer and tell the server.
            if *self.generation.lock().unwrap() != my_gen {
                let _ = ws.send(tungstenite::Message::Text(serde_json::to_string(
                    &ClientMsg::UploadCancel,
                )?));
                self.link.lock().unwrap().upload = None;
                return Ok(());
            }
            let n = reader.read(&mut buf).context("read model file")?;
            if n == 0 {
                break;
            }
            ws.send(tungstenite::Message::Binary(buf[..n].to_vec()))
                .context("send model chunk")?;
            sent += n as u64;
            if last_notify.elapsed() > Duration::from_millis(200) {
                set_progress(sent);
                last_notify = Instant::now();
            }
        }

        ws.send(tungstenite::Message::Text(serde_json::to_string(&ClientMsg::UploadEnd)?))
            .context("send UploadEnd")?;
        self.link.lock().unwrap().upload = None;
        self.notify();
        Ok(())
    }
}

/// The client's concrete WebSocket type (TLS over TCP).
type ClientWs = tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

// ---------------------------------------------------------------------------
// Fingerprint-pinning certificate verifier
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PinVerifier {
    pinned: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: rustls::crypto::CryptoProvider,
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = pairing::sha256(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fp);
        match self.pinned {
            Some(p) if p != fp => Err(rustls::Error::General(
                "server certificate fingerprint mismatch — re-pair or check for interception"
                    .into(),
            )),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
