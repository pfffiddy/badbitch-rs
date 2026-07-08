//! Drop-any-model ingestion pipeline.
//!
//! A dropped path is classified and routed:
//!   *.gguf file            → Modelfile + `ollama create`         (direct import)
//!   dir with *.safetensors → llama.cpp convert_hf_to_gguf.py
//!     or config.json         [→ llama-quantize]                  (convert, then import)
//!   registry name          → POST /api/pull                      (download)
//!
//! Each job runs on a worker thread and reports `IngestEvent`s to the UI.

use crate::backend::Backend;
use crate::protocol::Notify;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

#[derive(Debug)]
pub enum IngestEvent {
    Log(String),
    Progress(f32), // 0..=1 when known
    Done(String),  // resulting model name
    Error(String),
}

pub struct IngestHandle {
    pub events: Receiver<IngestEvent>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelSource {
    Gguf(PathBuf),
    HuggingFaceDir(PathBuf),
    Unknown(PathBuf),
}

pub fn classify(path: &Path) -> ModelSource {
    if path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf")) {
        return ModelSource::Gguf(path.to_path_buf());
    }
    if path.is_dir() {
        let has = |pred: &dyn Fn(&Path) -> bool| {
            std::fs::read_dir(path)
                .map(|rd| rd.flatten().any(|e| pred(&e.path())))
                .unwrap_or(false)
        };
        let safetensors =
            has(&|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("safetensors")));
        let config = has(&|p| p.file_name().is_some_and(|n| n == "config.json"));
        let bin = has(&|p| {
            p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("pytorch_model"))
        });
        if safetensors || (config && bin) || config {
            return ModelSource::HuggingFaceDir(path.to_path_buf());
        }
        // A dir that just contains a single .gguf
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")) {
                    return ModelSource::Gguf(e.path());
                }
            }
        }
    }
    ModelSource::Unknown(path.to_path_buf())
}

fn sanitize_name(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
        .collect();
    s.trim_matches('-').to_lowercase()
}

pub struct IngestJob {
    pub source: ModelSource,
    /// Path to a llama.cpp checkout (for convert_hf_to_gguf.py + llama-quantize).
    pub llama_cpp_dir: String,
    /// e.g. "Q4_K_M"; empty = keep the converted f16.
    pub quantize: String,
}

pub fn spawn_ingest(
    backend: Arc<dyn Backend>,
    job: IngestJob,
    notify: Notify,
) -> IngestHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();
    std::thread::spawn(move || {
        let r = run_job(&*backend, &job, &tx, &cancel2, &notify);
        if let Err(e) = r {
            let _ = tx.send(IngestEvent::Error(e.to_string()));
        }
        notify();
    });
    IngestHandle { events: rx, cancel }
}

pub fn spawn_pull(
    backend: Arc<dyn Backend>,
    name: String,
    notify: Notify,
) -> IngestHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();
    std::thread::spawn(move || {
        let _ = tx.send(IngestEvent::Log(format!("pulling '{name}' from registry…")));
        let mut last_status = String::new();
        let res = backend.pull_model(&name, &mut |status, frac| {
            if status != last_status {
                last_status = status.to_string();
                let _ = tx.send(IngestEvent::Log(status.to_string()));
            }
            if let Some(f) = frac {
                let _ = tx.send(IngestEvent::Progress(f));
            }
            notify();
            !cancel2.load(Ordering::Relaxed)
        });
        match res {
            Ok(()) => {
                let _ = tx.send(IngestEvent::Done(name));
            }
            Err(e) => {
                let _ = tx.send(IngestEvent::Error(e.to_string()));
            }
        }
        notify();
    });
    IngestHandle { events: rx, cancel }
}

fn run_job(
    backend: &dyn Backend,
    job: &IngestJob,
    tx: &Sender<IngestEvent>,
    cancel: &AtomicBool,
    notify: &Notify,
) -> Result<()> {
    let log = |m: String| {
        let _ = tx.send(IngestEvent::Log(m));
        notify();
    };

    match &job.source {
        ModelSource::Gguf(path) => {
            let name = sanitize_name(
                path.file_stem().map(|s| s.to_string_lossy().to_string()).as_deref().unwrap_or("imported"),
            );
            log(format!("detected GGUF → importing as '{name}'"));
            import(backend, path, &name, tx, cancel, notify)?;
            let _ = tx.send(IngestEvent::Done(name));
            Ok(())
        }
        ModelSource::HuggingFaceDir(dir) => {
            let base = sanitize_name(
                dir.file_name().map(|s| s.to_string_lossy().to_string()).as_deref().unwrap_or("converted"),
            );
            log(format!("detected HuggingFace/safetensors model dir → conversion pipeline"));

            // 1) convert_hf_to_gguf.py → f16 GGUF
            let convert = Path::new(&job.llama_cpp_dir).join("convert_hf_to_gguf.py");
            if !convert.exists() {
                bail!(
                    "convert_hf_to_gguf.py not found at {} — set the llama.cpp path in the \
                     Model Import panel (git clone https://github.com/ggml-org/llama.cpp)",
                    convert.display()
                );
            }
            let out_dir = std::env::temp_dir().join("llm-desk-convert");
            std::fs::create_dir_all(&out_dir)?;
            let f16 = out_dir.join(format!("{base}-f16.gguf"));
            log(format!("converting → {}", f16.display()));
            run_streamed(
                Command::new("python3")
                    .arg(&convert)
                    .arg(dir)
                    .args(["--outtype", "f16", "--outfile"])
                    .arg(&f16),
                tx,
                cancel,
                notify,
            )
            .context("conversion failed (do you have llama.cpp's python requirements installed?)")?;

            // 2) optional quantization
            let final_path = if job.quantize.trim().is_empty() {
                f16.clone()
            } else {
                let q = job.quantize.trim().to_uppercase();
                let quantized = out_dir.join(format!("{base}-{}.gguf", q.to_lowercase()));
                let quant_bin = find_quantize_bin(&job.llama_cpp_dir)
                    .context("llama-quantize binary not found — build llama.cpp first (see README)")?;
                log(format!("quantizing to {q} → {}", quantized.display()));
                run_streamed(
                    Command::new(quant_bin).arg(&f16).arg(&quantized).arg(&q),
                    tx,
                    cancel,
                    notify,
                )?;
                quantized
            };

            // 3) import into Ollama
            let name = if job.quantize.trim().is_empty() {
                base
            } else {
                format!("{base}-{}", job.quantize.trim().to_lowercase())
            };
            log(format!("importing '{name}' into Ollama"));
            import(backend, &final_path, &name, tx, cancel, notify)?;
            let _ = tx.send(IngestEvent::Done(name));
            Ok(())
        }
        ModelSource::Unknown(p) => bail!(
            "couldn't identify a model at {} — expected a .gguf file or a HuggingFace \
             directory (config.json / *.safetensors)",
            p.display()
        ),
    }
}

fn import(
    backend: &dyn Backend,
    path: &Path,
    name: &str,
    tx: &Sender<IngestEvent>,
    cancel: &AtomicBool,
    notify: &Notify,
) -> Result<()> {
    backend.import_gguf(&path.to_string_lossy(), name, &mut |msg, frac| {
        let _ = tx.send(IngestEvent::Log(msg.to_string()));
        if let Some(f) = frac {
            let _ = tx.send(IngestEvent::Progress(f));
        }
        notify();
        !cancel.load(Ordering::Relaxed)
    })
}

fn find_quantize_bin(llama_dir: &str) -> Option<PathBuf> {
    for rel in ["build/bin/llama-quantize", "llama-quantize", "build/llama-quantize"] {
        let p = Path::new(llama_dir).join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    // maybe it's on PATH
    Command::new("llama-quantize")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success() || s.code() == Some(1))
        .map(|_| PathBuf::from("llama-quantize"))
}

/// Run a subprocess, streaming stdout+stderr lines to the log, honoring cancel.
fn run_streamed(
    cmd: &mut Command,
    tx: &Sender<IngestEvent>,
    cancel: &AtomicBool,
    notify: &Notify,
) -> Result<()> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let pump = |reader: Box<dyn std::io::Read + Send>, tx: Sender<IngestEvent>, notify: Notify| {
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                let _ = tx.send(IngestEvent::Log(line));
                notify();
            }
        })
    };
    let h1 = stdout.map(|s| pump(Box::new(s), tx.clone(), notify.clone()));
    let h2 = stderr.map(|s| pump(Box::new(s), tx.clone(), notify.clone()));

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            bail!("cancelled");
        }
        if let Some(status) = child.try_wait()? {
            if let Some(h) = h1 { let _ = h.join(); }
            if let Some(h) = h2 { let _ = h.join(); }
            if !status.success() {
                bail!("subprocess exited with {status}");
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}
