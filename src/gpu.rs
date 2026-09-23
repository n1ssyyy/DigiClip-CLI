//! GPU inventory for the transcription toggle.
//!
//! Port of `App\Services\System\GpuDetector`: detect everything,
//! rank discrete-VRAM beasts first, transcribe on the winner.
//! Strategies are per-OS and best-effort; unknown output is skipped,
//! never fatal.
//!
//! Windows: `nvidia-smi` CSV + PowerShell `Get-CimInstance
//! Win32_VideoController` (WMI). `AdapterRAM` is uint32 and saturates at
//! 4294967295 on 4GB+ cards -> memory reported as `None` in that case.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDevice {
    pub name: String,
    pub memory_mb: Option<u64>,
    pub discrete: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct VulkanDevice {
    pub index: usize,
    pub name: String,
}

pub fn looks_discrete(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "rtx", "gtx", "quadro", "tesla", "titan", "geforce", "radeon", "arc a",
    ]
    .iter()
    .any(|k| n.contains(k))
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = crate::process::command(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`.
/// The GPU name itself may contain commas — memory is the last field.
pub fn from_nvidia_smi() -> Vec<GpuDevice> {
    let Some(out) = run(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return vec![];
    };
    let mut gpus = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(pos) = line.rfind(',') else { continue };
        let name = line[..pos].trim().to_string();
        let mem_digits: String = line[pos + 1..]
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        let mem: u64 = mem_digits.parse().unwrap_or(0);
        if name.is_empty() {
            continue;
        }
        gpus.push(GpuDevice {
            name,
            memory_mb: if mem > 0 { Some(mem) } else { None },
            discrete: true,
            source: "nvidia-smi".into(),
        });
    }
    gpus
}

/// Windows WMI fallback. Runs powershell (not pwsh) so it works on stock
/// Windows 10/11 without PowerShell Core installed.
pub fn from_wmi() -> Vec<GpuDevice> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "AdapterRAM")]
        ram: Option<i64>,
    }
    let out = crate::process::command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_VideoController | Select-Object Name,AdapterRAM | ConvertTo-Json -Compress",
        ])
        .output();
    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return vec![];
    }
    // Single object vs array.
    let rows: Vec<Row> = if text.trim_start().starts_with('[') {
        serde_json::from_str(&text).unwrap_or_default()
    } else {
        match serde_json::from_str::<Row>(&text) {
            Ok(r) => vec![r],
            Err(_) => vec![],
        }
    };
    rows.into_iter()
        .filter_map(|r| {
            let name = r.name.unwrap_or_default().trim().to_string();
            if name.is_empty() {
                return None;
            }
            let ram = r.ram.unwrap_or(0);
            // uint32 saturates at 4294967295 on 4GB+ cards.
            let mem = if ram > 0 && (ram as u64) < 4294967295 {
                Some((ram as u64) / 1048576)
            } else {
                None
            };
            let discrete = looks_discrete(&name);
            Some(GpuDevice {
                name,
                memory_mb: mem,
                discrete,
                source: "wmi".into(),
            })
        })
        .collect()
}

fn from_lspci() -> Vec<GpuDevice> {
    let Some(out) = run("sh", &["-c", "lspci -mm | grep -i -E 'vga|3d|display'"]) else {
        return vec![];
    };
    let mut gpus = Vec::new();
    for line in out.lines() {
        // -mm quotes every field: "01:00.0" "3D controller" "NVIDIA ..." "GA107M [...]"
        let parts: Vec<&str> = line
            .split('"')
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .map(|(_, s)| s)
            .collect();
        if parts.len() < 3 {
            continue;
        }
        let class = parts[1];
        let name = if !parts[3.min(parts.len() - 1)].trim().is_empty() && parts.len() > 3 {
            parts[3].trim().to_string()
        } else {
            parts[2].trim().to_string()
        };
        if name.is_empty() {
            continue;
        }
        let discrete = class.to_lowercase().contains("3d") || looks_discrete(&name);
        gpus.push(GpuDevice {
            name,
            memory_mb: None,
            discrete,
            source: "lspci".into(),
        });
    }
    gpus
}

/// Discrete first, then most VRAM (unknown last), then name.
pub fn rank(mut gpus: Vec<GpuDevice>) -> Vec<GpuDevice> {
    gpus.sort_by(|a, b| {
        (
            (!a.discrete) as u8,
            a.memory_mb.map(|m| -(m as i64)).unwrap_or(i64::MAX),
            a.name.clone(),
        )
            .cmp(&(
                (!b.discrete) as u8,
                b.memory_mb.map(|m| -(m as i64)).unwrap_or(i64::MAX),
                b.name.clone(),
            ))
    });
    gpus
}

pub fn probe() -> Vec<GpuDevice> {
    let mut found = from_nvidia_smi();
    if cfg!(windows) {
        found.extend(from_wmi());
    } else if cfg!(target_os = "linux") {
        found.extend(from_lspci());
    }
    // De-dupe by case-insensitive name, prefer nvidia-smi entries.
    let mut seen = std::collections::HashSet::new();
    let mut uniq = Vec::new();
    for g in rank(found) {
        let k = g.name.to_lowercase();
        if seen.insert(k) {
            uniq.push(g);
        }
    }
    rank(uniq)
}

pub fn best() -> Option<GpuDevice> {
    probe().into_iter().next()
}

fn name_tokens(name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let mut out = Vec::new();
    for tok in lower.split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        match tok {
            "graphics" | "laptop" | "mobile" | "corporation" | "inc" | "corp" | "co" | "ltd"
            | "the" => continue,
            _ => {}
        }
        if !out.iter().any(|t| t == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Token-overlap 0..1 of candidate against reference (both GPU names).
/// V2 hook: maps the ranked-best GPU to whisper's `-dev N` index.
#[allow(dead_code)]
pub fn name_score(reference: &str, candidate: &str) -> f64 {
    let r = name_tokens(reference);
    if r.is_empty() {
        return 0.0;
    }
    let cand_owned: std::collections::HashSet<String> =
        name_tokens(candidate).into_iter().collect();
    let mut hits = 0;
    for tok in &r {
        if cand_owned.contains(tok) {
            hits += 1;
        }
    }
    hits as f64 / r.len() as f64
}

/// Parse `ggml_vulkan: 1 = NVIDIA GeForce RTX ... | uma: ...` lines.
/// V2 hook: device enumeration for `-dev` mapping.
#[allow(dead_code)]
pub fn parse_vulkan_devices(output: &str) -> Vec<VulkanDevice> {
    let mut devices = Vec::new();
    for line in output.lines() {
        // ggml_vulkan: 1 = NAME (VENDOR) | ...
        if let Some((_, rest)) = line.split_once("ggml_vulkan:") {
            let rest = rest.trim();
            if let Some((idx_s, name_s)) = rest.split_once('=') {
                if let Ok(index) = idx_s.trim().parse::<usize>() {
                    let name = name_s.split('|').next().unwrap_or("").trim().to_string();
                    if !name.is_empty() {
                        devices.push(VulkanDevice { index, name });
                    }
                }
            }
        }
    }
    devices
}

/// Map our ranked-best GPU to whisper's `-dev N` index.
/// Null (None) = unknown, caller omits -dev (whisper default). Never throws.
/// V2 hook: needs a live sidecar stderr sample; v1 omits -dev.
#[allow(dead_code)]
pub fn preferred_vulkan_index(
    vulkan_binary: &std::path::Path,
    stderr_sample: &str,
) -> Option<usize> {
    let b = best()?;
    let devices = parse_vulkan_devices(stderr_sample);
    if devices.is_empty() {
        let _ = vulkan_binary;
        return None;
    }
    let mut scored: Vec<(f64, usize)> = devices
        .iter()
        .map(|d| (name_score(&b.name, &d.name), d.index))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    // >= 0.5 = at least half the best-GPU tokens matched.
    if scored[0].0 >= 0.5 {
        Some(scored[0].1)
    } else {
        None
    }
}

/// Master GPU switch (one policy for the whole app): when ON, every step
/// with a GPU path uses it (Vulkan STT sidecar, DirectML tracking, NVENC
/// renders); when OFF, everything is CPU (embedded STT, CPU ort, libx264).
/// Resolves to `(on, human_reason)`. Never fails — worst case is all-CPU.
pub fn resolve_mode(want_gpu: bool) -> (bool, String) {
    if !want_gpu {
        return (false, "--no-gpu: all-CPU mode".into());
    }
    match best() {
        Some(g) => (
            true,
            format!(
                "GPU mode ON ({}, {})",
                g.name,
                g.memory_mb
                    .map(|m| format!("{m}MB"))
                    .unwrap_or_else(|| "?MB".into())
            ),
        ),
        None => (false, "no GPU detected: all-CPU mode".into()),
    }
}
