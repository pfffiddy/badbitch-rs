//! Subprocess helpers — `_have` (badbitch2.py:237) and a timed command runner used by the
//! CLI-backed tools (sherlock, holehe, and run_shell in Phase 2).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// `_have` (badbitch2.py:237): is `binary` on PATH?
pub async fn have(binary: &str) -> bool {
    Command::new("which")
        .arg(binary)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Run a program with args, capturing stdout/stderr, killed after `timeout_secs`.
pub async fn run(program: &str, args: &[&str], timeout_secs: u64) -> std::io::Result<Output> {
    let child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(res) => {
            let out = res?;
            Ok(Output {
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                timed_out: false,
            })
        }
        Err(_) => Ok(Output {
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        }),
    }
}

/// Run a shell command line (`sh -c`) with a working dir, capturing combined output.
pub async fn run_shell_line(command: &str, cwd: &std::path::Path, timeout_secs: u64) -> Output {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            return Output {
                stdout: String::new(),
                stderr: format!("[shell error] {e}"),
                timed_out: false,
            };
        }
    };
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(out)) => Output {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            timed_out: false,
        },
        Ok(Err(e)) => Output {
            stdout: String::new(),
            stderr: format!("[shell error] {e}"),
            timed_out: false,
        },
        Err(_) => Output {
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        },
    }
}
