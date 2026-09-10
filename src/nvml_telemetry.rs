//! Persistent NVML session. No subprocesses and no synthetic zero activity.
use crate::{
    gpu_process::Usage,
    model::unix_ms,
    telemetry::{GpuUsage, ProcessUsage},
};
use nvml_wrapper::{
    Nvml,
    enums::device::UsedGpuMemory,
    error::NvmlError,
    struct_wrappers::device::{ProcessInfo, ProcessUtilizationSample},
};
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Collector {
    nvml: Option<Nvml>,
    last_init: Option<Instant>,
    cursors: HashMap<String, u64>,
    identities: HashMap<u32, u64>,
}

#[derive(Default)]
pub struct Sample {
    pub devices: Vec<GpuUsage>,
    pub processes: HashMap<u32, Vec<Usage>>,
    pub supported: bool,
}

fn memory_by_pid(processes: impl IntoIterator<Item = ProcessInfo>) -> HashMap<u32, Option<u64>> {
    let mut memory: HashMap<u32, Option<u64>> = HashMap::new();
    for process in processes {
        let value = match process.used_gpu_memory {
            UsedGpuMemory::Used(bytes) => Some(bytes),
            UsedGpuMemory::Unavailable => None,
        };
        // Compute and graphics inventories can contain the same PID and memory.
        let entry = memory.entry(process.pid).or_default();
        *entry = (*entry).max(value);
    }
    memory
}

fn latest_samples(
    samples: Vec<ProcessUtilizationSample>,
    after: u64,
    now_us: u64,
) -> HashMap<u32, ProcessUtilizationSample> {
    let mut latest: HashMap<u32, ProcessUtilizationSample> = HashMap::new();
    for sample in samples {
        if sample.timestamp <= after
            || sample.timestamp < now_us.saturating_sub(6_000_000)
            || sample.timestamp > now_us
            || [sample.sm_util, sample.enc_util, sample.dec_util]
                .iter()
                .any(|value| *value > 100)
        {
            continue;
        }
        if latest
            .get(&sample.pid)
            .is_none_or(|old| old.timestamp < sample.timestamp)
        {
            latest.insert(sample.pid, sample);
        }
    }
    latest
}

impl Collector {
    pub fn sample(&mut self, processes: &[ProcessUsage]) -> Sample {
        if self.nvml.is_none()
            && self
                .last_init
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            self.last_init = Some(Instant::now());
            self.nvml = Nvml::init().ok();
            self.cursors.clear();
            self.identities.clear();
        }
        let Some(nvml) = &self.nvml else {
            return Sample::default();
        };
        let Ok(count) = nvml.device_count() else {
            self.nvml = None;
            return Sample::default();
        };
        let mut result = Sample::default();
        let mut next_cursors = HashMap::new();
        for index in 0..count.min(32) {
            let Ok(device) = nvml.device_by_index(index) else {
                continue;
            };
            let Ok(uuid) = device.uuid() else {
                continue;
            };
            let memory = device.memory_info().ok();
            let utilization = device.utilization_rates().ok();
            result.devices.push(GpuUsage {
                name: device
                    .name()
                    .unwrap_or_else(|_| format!("NVIDIA GPU {index}")),
                scope: "device",
                source: "NVML",
                busy_percent: utilization.map(|value| f64::from(value.gpu)),
                memory_used_bytes: memory.as_ref().map(|value| value.used),
                memory_total_bytes: memory.as_ref().map(|value| value.total),
                note: None,
            });
            let compute = device.running_compute_processes();
            let graphics = device.running_graphics_processes();
            let inventory_available = compute.is_ok() || graphics.is_ok();
            let memory = memory_by_pid(
                compute
                    .unwrap_or_default()
                    .into_iter()
                    .chain(graphics.unwrap_or_default()),
            );
            // Start near the current window, rather than replaying driver history.
            let after = self
                .cursors
                .get(&uuid)
                .copied()
                .unwrap_or_else(|| unix_ms().saturating_sub(2000) * 1000);
            let (samples, activity_status, activity_supported) =
                match device.process_utilization_stats(after) {
                    Ok(samples) => (samples, "no_new_sample", true),
                    Err(NvmlError::NotFound) => (Vec::new(), "no_new_sample", true),
                    Err(NvmlError::NotSupported | NvmlError::FunctionNotFound) => {
                        (Vec::new(), "unsupported", false)
                    }
                    Err(_) => (Vec::new(), "unavailable", false),
                };
            let now = unix_ms();
            let now_us = now * 1000 + 999;
            let cursor = samples
                .iter()
                .filter(|sample| sample.timestamp <= now_us)
                .map(|sample| sample.timestamp)
                .max()
                .unwrap_or(after)
                .max(after);
            next_cursors.insert(uuid, cursor);
            let latest = latest_samples(samples, after, now_us);
            result.supported |= inventory_available || activity_supported;
            for process in processes {
                let same_process = self.identities.get(&process.pid) == Some(&process.start_ticks);
                let activity = latest.get(&process.pid).filter(|_| same_process);
                if !memory.contains_key(&process.pid) && activity.is_none() {
                    continue;
                }
                let engines = BTreeMap::from([
                    ("SM".into(), activity.map(|s| f64::from(s.sm_util))),
                    ("encode".into(), activity.map(|s| f64::from(s.enc_util))),
                    ("decode".into(), activity.map(|s| f64::from(s.dec_util))),
                ]);
                result
                    .processes
                    .entry(process.pid)
                    .or_default()
                    .push(Usage {
                        device: format!("NVIDIA GPU {index}"),
                        source: "NVML",
                        engines,
                        memory_bytes: memory.get(&process.pid).copied().flatten(),
                        memory_kind: "framebuffer memory",
                        sampled_at_ms: now,
                        activity_status: Some(if activity.is_some() {
                            "sampled"
                        } else if !same_process {
                            "warming_up"
                        } else {
                            activity_status
                        }),
                        activity_sample_at_ms: activity.map(|s| s.timestamp / 1000),
                    });
            }
        }
        self.cursors = next_cursors;
        self.identities = processes.iter().map(|p| (p.pid, p.start_ticks)).collect();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the local NVIDIA driver; measures read-only steady-state sampling"]
    fn live_nvml_persistent_sampling() {
        let processes: Vec<_> = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                let name = std::fs::read_to_string(entry.path().join("comm")).ok()?;
                if name.trim() != "tarsier-percept" {
                    return None;
                }
                Some(ProcessUsage {
                    pid,
                    start_ticks: crate::gpu_process::current_start(pid)?,
                    name: name.trim().into(),
                    role: "Perception",
                    cpu_percent: None,
                    rss_bytes: 0,
                    gpus: Vec::new(),
                })
            })
            .collect();
        let mut collector = Collector::default();
        assert!(!collector.sample(&processes).devices.is_empty());
        fn cpu_seconds() -> f64 {
            // SAFETY: getrusage writes to a properly sized, initialized output struct.
            let usage = unsafe {
                let mut usage: nix::libc::rusage = std::mem::zeroed();
                assert_eq!(nix::libc::getrusage(nix::libc::RUSAGE_SELF, &mut usage), 0);
                usage
            };
            (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
                + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
        }
        let cpu = cpu_seconds();
        let start = Instant::now();
        for _ in 0..50 {
            collector.sample(&processes);
        }
        println!(
            "NVML steady-state: {:.3} ms CPU / {:.3} ms elapsed per full poll",
            (cpu_seconds() - cpu) * 1000.0 / 50.0,
            start.elapsed().as_secs_f64() * 1000.0 / 50.0
        );
        for _ in 0..3 {
            std::thread::sleep(Duration::from_secs(2));
            let sample = collector.sample(&processes);
            assert!(!sample.devices.is_empty());
            println!(
                "NVML worker sample: {}",
                serde_json::to_string(&sample.processes).unwrap()
            );
        }
    }

    #[test]
    fn graphics_and_compute_memory_is_not_counted_twice() {
        let process = |bytes| ProcessInfo {
            pid: 42,
            used_gpu_memory: bytes,
            gpu_instance_id: None,
            compute_instance_id: None,
        };
        let values = memory_by_pid([
            process(UsedGpuMemory::Used(100)),
            process(UsedGpuMemory::Used(100)),
            process(UsedGpuMemory::Unavailable),
        ]);
        assert_eq!(values[&42], Some(100));
        assert_eq!(values.len(), 1);
    }
    #[test]
    fn activity_only_uses_fresh_valid_samples_and_keeps_reported_zero() {
        let sample = |timestamp, sm_util| ProcessUtilizationSample {
            pid: 42,
            timestamp,
            sm_util,
            mem_util: 0,
            enc_util: 0,
            dec_util: 0,
        };
        let latest = latest_samples(
            vec![
                sample(9_000_000, 30),
                sample(9_500_000, 0),
                sample(9_800_000, 101),
                sample(11_000_000, 50),
            ],
            8_000_000,
            10_000_000,
        );
        assert_eq!(latest[&42].sm_util, 0);
        assert!(latest_samples(vec![sample(9_000_000, 30)], 9_000_000, 10_000_000).is_empty());
        assert!(latest_samples(vec![sample(1_000_000, 30)], 0, 10_000_000).is_empty());
    }
}
