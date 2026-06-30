//! Shell-backed tools: run_shell (badbitch2.py:1093), python_eval (1107), exif_metadata (1121).

use badbitch_macros::tool;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::shell;
use crate::tool::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunShellInput {
    pub command: String,
}

#[tool(
    name = "run_shell",
    description = "Run a shell command and return stdout+stderr. Use for OSINT tools not covered by a dedicated tool: nmap, spiderfoot, recon-ng, amass, sublist3r, jq, sqlite3, curl, etc. Real shell on the host machine."
)]
pub async fn run_shell(ctx: ToolContext, input: RunShellInput) -> String {
    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let o = shell::run_shell_line(&input.command, &workdir, ctx.config.shell_timeout).await;
    let out = if o.stderr.is_empty() {
        o.stdout
    } else {
        format!("{}\n[stderr]\n{}", o.stdout, o.stderr)
    };
    if o.timed_out {
        return format!("[timeout after {}s] {}", ctx.config.shell_timeout, input.command);
    }
    let out = out.trim_end().to_string();
    if out.is_empty() { "[no output]".to_string() } else { crate::util::truncate_chars(&out, 8000) }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PythonEvalInput {
    pub code: String,
}

#[tool(
    name = "python_eval",
    description = "Execute Python 3 code via `python3 -c` and return stdout. Use for custom regex extractors, parsing JSON/HTML, transforming scraped/structured data, or quick computation. Keep code single-line-friendly or use semicolons."
)]
pub async fn python_eval(ctx: ToolContext, input: PythonEvalInput) -> String {
    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cmd = format!("python3 -c {}", shell_escape(&input.code));
    let o = shell::run_shell_line(&cmd, &workdir, ctx.config.shell_timeout).await;
    if o.timed_out {
        return format!("[python timeout after {}s]", ctx.config.shell_timeout);
    }
    let out = if o.stderr.trim().is_empty() {
        o.stdout
    } else {
        format!("{}\n[stderr]\n{}", o.stdout, o.stderr)
    };
    let out = out.trim_end().to_string();
    if out.is_empty() { "[ok, no stdout]".to_string() } else { crate::util::truncate_chars(&out, 6000) }
}

fn shell_escape(s: &str) -> String {
    // Single-quote wrap; escape any single quotes inside by ending the quote, inserting \',
    // and reopening — standard POSIX shell single-quote escaping.
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExifMetadataInput {
    pub path_or_url: String,
}

#[tool(
    name = "exif_metadata",
    description = "Run exiftool on a local image path OR an image URL (downloaded first). Surfaces GPS, capture timestamp, device, software — for geolocating/chronolocating a photo."
)]
pub async fn exif_metadata(ctx: ToolContext, input: ExifMetadataInput) -> String {
    if !shell::have("exiftool").await {
        return "[exiftool missing] sudo apt install -y libimage-exiftool-perl".to_string();
    }
    let path = if input.path_or_url.to_lowercase().starts_with("http") {
        // Download to a temp file
        let tmp = std::env::temp_dir().join(format!("exif_{}.img", std::process::id()));
        match crate::http::get(&ctx.http, &ctx.config, &input.path_or_url, &[], &[]).await {
            Ok(r) => {
                let bytes = match r.bytes().await {
                    Ok(b) => b,
                    Err(e) => return format!("[exif error] download failed: {e}"),
                };
                if let Err(e) = std::fs::write(&tmp, &bytes) {
                    return format!("[exif error] write temp failed: {e}");
                }
                tmp.to_string_lossy().to_string()
            }
            Err(e) => return format!("[exif error] fetch failed: {e}"),
        }
    } else {
        input.path_or_url.clone()
    };
    let out = match shell::run("exiftool", &["-n", "-G", &path], 60).await {
        Ok(o) => o.stdout,
        Err(e) => return format!("[exif error] {e}"),
    };
    // Clean up temp file if we created one
    if input.path_or_url.to_lowercase().starts_with("http") {
        let _ = std::fs::remove_file(&path);
    }
    let gps: Vec<&str> = out.lines().filter(|l| l.contains("GPS")).collect();
    let gps_block = if gps.is_empty() {
        "(no GPS tags)\n\n".to_string()
    } else {
        format!("GPS:\n{}\n\n", gps.join("\n"))
    };
    gps_block + &crate::util::truncate_chars(&out, 3000)
}
