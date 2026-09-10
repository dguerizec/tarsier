//! Low-frequency local resource samples. CPU 100% means one logical core.
use crate::{model::unix_ms, runtime::Runtime};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerStage {
    pub calls: u64,
    pub total_ms: f64,
    pub max_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerTelemetry {
    pub pid: u32,
    pub interval_ms: f64,
    pub stages: BTreeMap<String, WorkerStage>,
    #[serde(skip_deserializing)]
    pub received_at_ms: u64,
}

impl WorkerTelemetry {
    pub fn valid(&self) -> bool {
        const NAMES: &[&str] = &[
            "decode", "face", "hands", "pose", "segmentation", "observations_publish",
            "mask_publish", "avatar_tracking", "avatar_render", "avatar_publish",
            "depth", "depth_refine", "depth_publish",
        ];
        self.pid > 0 && self.interval_ms.is_finite() && self.interval_ms > 0.0
            && self.interval_ms <= 3_600_000.0 && self.stages.len() <= NAMES.len()
            && self.stages.iter().all(|(name, stage)| {
                NAMES.contains(&name.as_str()) && stage.calls <= 1_000_000
                    && stage.total_ms.is_finite() && (0.0..=3_600_000.0).contains(&stage.total_ms)
                    && if stage.calls == 0 {
                        stage.total_ms == 0.0 && stage.max_ms.is_none()
                    } else {
                        stage.max_ms.is_some_and(|max| max.is_finite() && max >= 0.0 && max <= stage.total_ms)
                    }
            })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Telemetry {
    pub sampled_at_ms: Option<u64>,
    pub interval_ms: u64,
    pub logical_cpus: usize,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub host_cpu_percent: Option<f64>,
    pub processes: Vec<ProcessUsage>,
    pub gpus: Vec<GpuUsage>,
    pub gpu_sampled_at_ms: Option<u64>,
    pub process_gpu_status: &'static str,
    pub partial: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProcessUsage {
    pub pid: u32,
    pub start_ticks: u64,
    pub gpus: Vec<crate::gpu_process::Usage>,
    pub name: String,
    pub role: &'static str,
    pub cpu_percent: Option<f64>,
    pub rss_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct GpuUsage {
    pub name: String,
    pub scope: &'static str,
    pub busy_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub source: &'static str,
    pub note: Option<String>,
}

#[derive(Clone, Debug)]
struct Stat {
    pid: u32,
    parent: u32,
    name: String,
    ticks: u64,
    start: u64,
    rss: u64,
}

fn parse_stat(pid: u32, text: &str, page_size: u64) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let fields: Vec<_> = text.get(close + 1..)?.split_whitespace().collect();
    Some(Stat {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        name: text.get(open + 1..close)?.into(),
        ticks: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .checked_add(fields.get(12)?.parse().ok()?)?,
        start: fields.get(19)?.parse().ok()?,
        rss: fields.get(21)?.parse::<i64>().ok()?.max(0) as u64 * page_size,
    })
}

fn descendants(stats: &BTreeMap<u32, Stat>, root: u32) -> HashSet<u32> {
    let mut selected = HashSet::from([root]);
    loop {
        let before = selected.len();
        for stat in stats.values() {
            if selected.contains(&stat.parent) {
                selected.insert(stat.pid);
            }
        }
        if selected.len() == before {
            return selected;
        }
    }
}

fn cpu_delta(previous: &Stat, current: &Stat, ticks_per_second: f64, elapsed: f64) -> Option<f64> {
    if previous.start != current.start || elapsed <= 0.0 {
        return None;
    }
    Some(current.ticks.checked_sub(previous.ticks)? as f64 / ticks_per_second / elapsed * 100.0)
}

fn host_ticks(text: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = text
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    Some((
        fields.iter().sum(),
        fields.get(3)? + fields.get(4).unwrap_or(&0),
    ))
}

#[derive(Default)]
struct Sampler {
    previous: HashMap<u32, Stat>,
    at: Option<Instant>,
    host: Option<(u64, u64)>,
}
impl Sampler {
    fn sample(&mut self, proc: &Path, root: u32) -> Telemetry {
        let now = Instant::now();
        let elapsed = self.at.map(|at| now.duration_since(at).as_secs_f64());
        let page_size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) }.max(1) as u64;
        let ticks = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) }.max(1) as f64;
        let logical_cpus =
            unsafe { nix::libc::sysconf(nix::libc::_SC_NPROCESSORS_ONLN) }.max(1) as usize;
        let mut sample = Telemetry {
            sampled_at_ms: Some(unix_ms()),
            interval_ms: elapsed.map(|e| (e * 1000.0) as u64).unwrap_or(0),
            logical_cpus,
            process_gpu_status: "not_collected",
            ..Default::default()
        };
        let mut stats = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(proc) else {
            sample.error = Some("Cannot read process metrics".into());
            return sample;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(text) => {
                    if let Some(stat) = parse_stat(pid, &text, page_size) {
                        stats.insert(pid, stat);
                    }
                }
                Err(error) => {
                    sample.partial |= error.kind() == std::io::ErrorKind::PermissionDenied;
                }
            }
        }
        if !stats.contains_key(&root) {
            sample.error = Some("Daemon process metrics unavailable".into());
        }
        let selected = descendants(&stats, root);
        let mut next = HashMap::new();
        for (pid, stat) in stats {
            if !selected.contains(&pid) {
                continue;
            }
            let cpu = elapsed.and_then(|elapsed| {
                self.previous
                    .get(&pid)
                    .and_then(|old| cpu_delta(old, &stat, ticks, elapsed))
            });
            let command =
                std::fs::read(proc.join(pid.to_string()).join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command);
            let role = if pid == root {
                "Daemon"
            } else if command.contains("tarsier-perception")
                || command.contains("tarsier_perception")
            {
                "Perception"
            } else if command.contains("voice") || command.contains("rvc") {
                "Voice"
            } else {
                "Worker"
            };
            sample.processes.push(ProcessUsage {
                pid,
                start_ticks: stat.start,
                gpus: Vec::new(),
                name: stat.name.clone(),
                role,
                cpu_percent: cpu,
                rss_bytes: stat.rss,
            });
            next.insert(pid, stat);
        }
        if !sample.processes.is_empty() {
            sample.rss_bytes = Some(sample.processes.iter().map(|p| p.rss_bytes).sum());
            let known: Vec<_> = sample
                .processes
                .iter()
                .filter_map(|p| p.cpu_percent)
                .collect();
            if !known.is_empty() {
                sample.cpu_percent = Some(known.iter().sum());
            }
        }
        if let Some(host) = std::fs::read_to_string(proc.join("stat"))
            .ok()
            .and_then(|s| host_ticks(&s))
        {
            sample.host_cpu_percent = self.host.and_then(|old| {
                let total = host.0.checked_sub(old.0)?;
                let idle = host.1.checked_sub(old.1)?;
                (total > 0).then(|| 100.0 * total.saturating_sub(idle) as f64 / total as f64)
            });
            self.host = Some(host);
        }
        self.previous = next;
        self.at = Some(now);
        sample
    }
}

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}
fn drm_gpus() -> Vec<GpuUsage> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name
            .strip_prefix("card")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let device = entry.path().join("device");
        let vendor = std::fs::read_to_string(device.join("vendor")).unwrap_or_default();
        let vendor = match vendor.trim() {
            "0x10de" => "NVIDIA",
            "0x1002" => "AMD",
            "0x8086" => "Intel",
            _ => "GPU",
        };
        let busy = number(&device.join("gpu_busy_percent"))
            .filter(|n| *n <= 100)
            .map(|n| n as f64);
        result.push(GpuUsage {
            name: format!("{vendor} {name}"),
            scope: "device",
            busy_percent: busy,
            memory_used_bytes: number(&device.join("mem_info_vram_used")),
            memory_total_bytes: number(&device.join("mem_info_vram_total")),
            source: "DRM sysfs",
            note: busy
                .is_none()
                .then(|| "Activity is not exposed by this driver".into()),
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

pub fn start(runtime: Runtime, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut sampler = Sampler::default();
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut process_collector = crate::gpu_process::Collector::default();
        let mut nvidia = crate::nvml_telemetry::Collector::default();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {}
            }
            if *shutdown.borrow() {
                break;
            }
            let Ok((returned, returned_process, returned_nvidia, sample)) = tokio::task::spawn_blocking(move || {
                let mut sample = sampler.sample(Path::new("/proc"), std::process::id());
                let nvidia_sample = nvidia.sample(&sample.processes);
                let mut gpus = drm_gpus();
                if !nvidia_sample.devices.is_empty() {
                    gpus.retain(|gpu| !gpu.name.starts_with("NVIDIA"));
                    gpus.extend(nvidia_sample.devices);
                }
                let (process_gpus, status) = process_collector.sample(&sample.processes, nvidia_sample.processes, nvidia_sample.supported);
                for process in &mut sample.processes {
                    process.gpus = process_gpus.get(&process.pid).cloned().unwrap_or_default();
                }
                sample.process_gpu_status = status;
                sample.gpus = gpus;
                sample.gpu_sampled_at_ms = Some(unix_ms());
                (sampler, process_collector, nvidia, sample)
            }).await else { break; };
            sampler = returned;
            process_collector = returned_process;
            nvidia = returned_nvidia;
            runtime.set_telemetry(sample).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stat(pid: u32, parent: u32, ticks: u64, start: u64) -> Stat {
        Stat {
            pid,
            parent,
            ticks,
            start,
            name: "worker".into(),
            rss: 4096,
        }
    }
    #[test]
    fn live_process_sampling_has_memory_and_a_cpu_delta() {
        let mut sampler = Sampler::default();
        let first = sampler.sample(Path::new("/proc"), std::process::id());
        assert!(first.error.is_none());
        assert!(first.rss_bytes.unwrap() > 0);
        assert!(first.cpu_percent.is_none());
        let second = sampler.sample(Path::new("/proc"), std::process::id());
        assert!(second.cpu_percent.is_some());
        assert!(
            second
                .processes
                .iter()
                .any(|p| p.pid == std::process::id() && p.role == "Daemon")
        );
    }

    #[test]
    fn cpu_counts_multiple_cores_and_rejects_pid_reuse() {
        assert_eq!(
            cpu_delta(&stat(1, 0, 10, 5), &stat(1, 0, 410, 5), 100.0, 2.0),
            Some(200.0)
        );
        assert_eq!(
            cpu_delta(&stat(1, 0, 10, 5), &stat(1, 0, 410, 6), 100.0, 2.0),
            None
        );
        assert_eq!(
            cpu_delta(&stat(1, 0, 410, 5), &stat(1, 0, 10, 5), 100.0, 2.0),
            None
        );
    }
    #[test]
    fn nested_workers_are_included_but_other_apps_are_not() {
        let stats = [
            (10, stat(10, 1, 0, 1)),
            (9, stat(9, 10, 0, 1)),
            (8, stat(8, 9, 0, 1)),
            (11, stat(11, 1, 0, 1)),
        ]
        .into();
        assert_eq!(descendants(&stats, 10), HashSet::from([10, 9, 8]));
    }
    #[test]
    fn proc_parser_handles_parentheses_and_guest_time() {
        let parsed = parse_stat(
            42,
            "42 (name with ) spaces) S 1 0 0 0 0 0 0 0 0 0 20 10 0 0 0 0 0 0 123 4096 2",
            4096,
        )
        .unwrap();
        assert_eq!(
            (parsed.parent, parsed.ticks, parsed.start, parsed.rss),
            (1, 30, 123, 8192)
        );
        assert_eq!(host_ticks("cpu 10 0 20 60 10 0 0 0 9 0\n"), Some((100, 70)));
        assert!(parse_stat(1, "broken", 4096).is_none());
    }

}
