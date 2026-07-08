//! Hardware detection + auto-tuning.
//!
//! Every tunable is an `OverrideField`: the UI shows a text box whose *hint*
//! is the auto-tuned value. Blank box → auto wins; typed value → manual wins.
//! Resolution is strictly per-field.

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    pub cpu_model: String,
    pub logical_cpus: usize,
    pub physical_cores: usize,
    pub ram_total_mb: u64,
    pub ram_available_mb: u64,
    pub gpu: Option<GpuInfo>,
}

impl SystemInfo {
    pub fn detect() -> Self {
        let logical_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();

        let cpu_model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown CPU".into());

        // Physical cores = unique (physical id, core id) pairs; fall back to logical.
        let mut pairs = std::collections::HashSet::new();
        let (mut phys, mut core) = (None::<&str>, None::<&str>);
        for l in cpuinfo.lines() {
            if let Some(v) = l.strip_prefix("physical id") {
                phys = v.split(':').nth(1).map(str::trim);
            } else if let Some(v) = l.strip_prefix("core id") {
                core = v.split(':').nth(1).map(str::trim);
            } else if l.trim().is_empty() {
                if let (Some(p), Some(c)) = (phys, core) {
                    pairs.insert((p.to_string(), c.to_string()));
                }
                phys = None;
                core = None;
            }
        }
        let physical_cores = if pairs.is_empty() { logical_cpus } else { pairs.len() };

        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mem_kb = |key: &str| -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };

        Self {
            cpu_model,
            logical_cpus,
            physical_cores,
            ram_total_mb: mem_kb("MemTotal") / 1024,
            ram_available_mb: mem_kb("MemAvailable") / 1024,
            gpu: detect_gpu(),
        }
    }
}

fn detect_gpu() -> Option<GpuInfo> {
    // NVIDIA
    if let Ok(out) = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total,memory.free", "--format=csv,noheader,nounits"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = s.lines().next() {
                let f: Vec<&str> = line.split(',').map(str::trim).collect();
                if f.len() >= 3 {
                    return Some(GpuInfo {
                        name: f[0].to_string(),
                        vram_total_mb: f[1].parse().unwrap_or(0),
                        vram_free_mb: f[2].parse().unwrap_or(0),
                    });
                }
            }
        }
    }
    // AMD (ROCm) — presence only; VRAM parsing varies by rocm-smi version.
    if let Ok(out) = Command::new("rocm-smi").arg("--showproductname").output() {
        if out.status.success() {
            return Some(GpuInfo {
                name: "AMD GPU (ROCm detected — VRAM not auto-read, set num_gpu manually if needed)"
                    .into(),
                vram_total_mb: 0,
                vram_free_mb: 0,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Parameter overrides — the app does NOT tune anything itself.
// ---------------------------------------------------------------------------
//
// Every field is a raw text box. Blank means the key is OMITTED from the
// Ollama `options` object entirely, so Ollama applies its own defaults and
// model-level settings. A value means "force exactly this".

/// UI-facing field: `manual` is the raw text box content ("" = let Ollama decide).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverrideField {
    pub manual: String,
}

impl OverrideField {
    pub fn is_manual(&self) -> bool {
        !self.manual.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParamOverrides {
    pub num_gpu: OverrideField,
    pub num_thread: OverrideField,
    pub num_ctx: OverrideField,
    pub temperature: OverrideField,
    pub top_p: OverrideField,
    pub top_k: OverrideField,
    pub repeat_penalty: OverrideField,
    pub num_predict: OverrideField,
}

impl ParamOverrides {
    /// Build the Ollama `options` object from explicitly set fields ONLY.
    /// Returns `{}` when nothing is overridden — Ollama tunes itself.
    pub fn to_options(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        let mut put_i = |k: &str, f: &OverrideField| {
            if let Ok(v) = f.manual.trim().parse::<i64>() {
                o.insert(k.into(), v.into());
            }
        };
        put_i("num_gpu", &self.num_gpu);
        put_i("num_thread", &self.num_thread);
        put_i("num_ctx", &self.num_ctx);
        put_i("top_k", &self.top_k);
        put_i("num_predict", &self.num_predict);
        let mut put_f = |k: &str, f: &OverrideField| {
            if let Ok(v) = f.manual.trim().parse::<f64>() {
                o.insert(k.into(), v.into());
            }
        };
        put_f("temperature", &self.temperature);
        put_f("top_p", &self.top_p);
        put_f("repeat_penalty", &self.repeat_penalty);
        serde_json::Value::Object(o)
    }
}
