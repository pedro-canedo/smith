use std::time::Duration;

use smith_core::{AgentEvent, ResourceStats};
use sysinfo::System;
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Polls local machine + GPU stats for as long as smith runs, for the
/// Ollama-only resource panel (token-billed providers show a cost estimate
/// instead — there's nothing local to measure there).
pub async fn poll(event_tx: mpsc::UnboundedSender<AgentEvent>, model: String) {
    // `System::new()` (not `new_all()`): we only ever read global CPU/memory
    // totals, never per-process data, so there's no reason to populate (and
    // keep refreshing) a full process table — `new_all()` was opening file
    // descriptors against every process on the machine on each refresh and
    // never releasing them.
    let mut sys = System::new();
    let mut interval = tokio::time::interval(POLL_INTERVAL);

    loop {
        interval.tick().await;

        sys.refresh_cpu_usage();
        sys.refresh_memory();

        let cpu_percent = sys.global_cpu_usage();
        let ram_used_mb = sys.used_memory() / 1024 / 1024;
        let ram_total_mb = sys.total_memory() / 1024 / 1024;

        let (gpu_percent, mut vram_used_mb, mut vram_total_mb) =
            nvidia_smi_stats().await.unwrap_or((None, None, None));
        if vram_used_mb.is_none() {
            if let Some((used, total)) = ollama_model_vram(&model).await {
                vram_used_mb = Some(used);
                vram_total_mb = total;
            }
        }

        let stats = ResourceStats {
            cpu_percent,
            ram_used_mb,
            ram_total_mb,
            gpu_percent,
            vram_used_mb,
            vram_total_mb,
        };
        if event_tx.send(AgentEvent::ResourceUsage(stats)).is_err() {
            return; // TUI has shut down
        }
    }
}

/// Best-effort: only works with an NVIDIA GPU and `nvidia-smi` on PATH.
/// Returns `None` silently on any other setup (AMD, Apple Silicon, no GPU, ...).
async fn nvidia_smi_stats() -> Option<(Option<f32>, Option<u64>, Option<u64>)> {
    let output = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let first_line = text.lines().next()?;
    let parts: Vec<&str> = first_line.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let gpu_percent = parts[0].parse::<f32>().ok();
    let used_mb = parts[1].parse::<u64>().ok();
    let total_mb = parts[2].parse::<u64>().ok();
    Some((gpu_percent, used_mb, total_mb))
}

/// Falls back to Ollama's own `/api/ps` (loaded-model memory footprint) when
/// `nvidia-smi` isn't available — covers AMD/Metal-backed Ollama setups too.
async fn ollama_model_vram(model: &str) -> Option<(u64, Option<u64>)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/api/ps", smith_persist::OLLAMA_HOST))
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let models = json.get("models")?.as_array()?;
    let entry = models
        .iter()
        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(model))?;
    let size_vram = entry.get("size_vram")?.as_u64()?;
    let size = entry.get("size").and_then(|v| v.as_u64());
    Some((size_vram / 1024 / 1024, size.map(|s| s / 1024 / 1024)))
}
