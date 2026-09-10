//! Per-process GPU metrics from NVIDIA pmon and standard DRM fdinfo counters.
use crate::{model::unix_ms, telemetry::ProcessUsage};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
pub struct Usage {
    pub device: String,
    pub source: &'static str,
    pub engines: BTreeMap<String, Option<f64>>,
    pub memory_bytes: Option<u64>,
    pub memory_kind: &'static str,
    pub sampled_at_ms: u64,
}

#[derive(Default)]
struct Client {
    id: String,
    device: String,
    engines: BTreeMap<String, u64>,
    capacities: BTreeMap<String, u64>,
    memory: Option<u64>,
    memory_kind: &'static str,
}
fn bytes(value: &str) -> Option<u64> {
    let mut words = value.split_whitespace();
    let number = words.next()?.parse::<u64>().ok()?;
    number.checked_mul(match words.next() {
        None | Some("B") => 1,
        Some("KiB") => 1024,
        Some("MiB") => 1048576,
        _ => return None,
    })
}
fn parse_fdinfo(text: &str) -> Option<Client> {
    let fields: BTreeMap<_, _> = text
        .lines()
        .filter_map(|s| s.split_once(':').map(|(k, v)| (k.trim(), v.trim())))
        .collect();
    if fields
        .get("drm-driver")
        .is_some_and(|driver| driver.starts_with("nvidia"))
    {
        return None; // NVIDIA is collected through pmon, including graphics contexts.
    }
    let mut client = Client {
        id: fields.get("drm-client-id")?.to_string(),
        device: fields
            .get("drm-pdev")
            .or_else(|| fields.get("drm-driver"))?
            .to_string(),
        ..Default::default()
    };
    // Choose one memory family to avoid counting resident/allocated totals twice.
    for (prefix, kind) in [
        ("drm-resident-", "resident GPU buffers"),
        ("drm-memory-", "allocated GPU buffers"),
        ("drm-total-", "allocated GPU buffers"),
    ] {
        let values: Vec<_> = fields
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .filter_map(|(_, v)| bytes(v))
            .collect();
        if !values.is_empty() {
            client.memory = Some(values.iter().sum());
            client.memory_kind = kind;
            break;
        }
    }
    for (key, value) in fields {
        if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
            if let Ok(capacity) = value.parse::<u64>() {
                client.capacities.insert(engine.into(), capacity.max(1));
            }
        } else if let Some(engine) = key.strip_prefix("drm-engine-") {
            let mut words = value.split_whitespace();
            if let (Some(value), Some("ns")) = (words.next(), words.next())
                && let Ok(value) = value.parse()
            {
                client.engines.insert(engine.into(), value);
            }
        }
    }
    Some(client)
}

pub fn parse_pmon(text: &str) -> Option<HashMap<u32, Vec<Usage>>> {
    let mut header = Vec::new();
    let mut result: HashMap<u32, Vec<Usage>> = HashMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.starts_with(&["#", "gpu", "pid"]) {
            header = fields[1..].to_vec();
            continue;
        }
        if line.trim_start().starts_with('#') || header.is_empty() {
            continue;
        }
        let get = |key: &str| {
            header
                .iter()
                .position(|s| *s == key)
                .and_then(|i| fields.get(i))
                .copied()
        };
        let Some(pid) = get("pid").and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let mut engines = BTreeMap::new();
        for (column, name) in [
            ("sm", "SM"),
            ("enc", "encode"),
            ("dec", "decode"),
            ("jpg", "JPEG"),
            ("ofa", "optical flow"),
        ] {
            if let Some(value) = get(column) {
                engines.insert(
                    name.into(),
                    value
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n)),
                );
            }
        }
        result.entry(pid).or_default().push(Usage {
            device: format!("NVIDIA GPU {}", get("gpu")?),
            source: "nvidia-smi pmon",
            engines,
            memory_bytes: get("fb")
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|v| v.checked_mul(1048576)),
            memory_kind: "framebuffer memory",
            sampled_at_ms: unix_ms(),
        });
    }
    (!header.is_empty()).then_some(result)
}

type CounterKey = (u32, u64, String, String, String);
#[derive(Default)]
pub struct Collector {
    previous: HashMap<CounterKey, (u64, Instant)>,
}
impl Collector {
    fn drm(&mut self, proc_root: &Path, processes: &[ProcessUsage]) -> HashMap<u32, Vec<Usage>> {
        let now = Instant::now();
        let mut next = HashMap::new();
        let mut result = HashMap::new();
        for process in processes {
            let Ok(entries) =
                std::fs::read_dir(proc_root.join(process.pid.to_string()).join("fdinfo"))
            else {
                continue;
            };
            let mut seen = HashSet::new();
            let mut devices: BTreeMap<String, Usage> = BTreeMap::new();
            for entry in entries.flatten() {
                let Some(client) = std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|text| parse_fdinfo(&text))
                else {
                    continue;
                };
                // A duplicated descriptor refers to the same DRM client.
                if !seen.insert((client.device.clone(), client.id.clone())) {
                    continue;
                }
                let usage = devices
                    .entry(client.device.clone())
                    .or_insert_with(|| Usage {
                        device: client.device.clone(),
                        source: "DRM fdinfo",
                        engines: BTreeMap::new(),
                        memory_bytes: None,
                        memory_kind: client.memory_kind,
                        sampled_at_ms: unix_ms(),
                    });
                if let Some(memory) = client.memory {
                    usage.memory_bytes =
                        Some(usage.memory_bytes.unwrap_or(0).saturating_add(memory));
                }
                for (engine, counter) in client.engines {
                    let key = (
                        process.pid,
                        process.start_ticks,
                        client.device.clone(),
                        client.id.clone(),
                        engine.clone(),
                    );
                    let value = self.previous.get(&key).and_then(|(old, at)| {
                        let elapsed = now.duration_since(*at).as_nanos() as f64;
                        let delta = counter.checked_sub(*old)?;
                        (elapsed > 0.0).then(|| {
                            (delta as f64
                                / elapsed
                                / (*client.capacities.get(&engine).unwrap_or(&1) as f64)
                                * 100.0)
                                .min(100.0)
                        })
                    });
                    next.insert(key, (counter, now));
                    let entry = usage.engines.entry(engine).or_insert(Some(0.0));
                    *entry = match (*entry, value) {
                        (Some(a), Some(b)) => Some((a + b).min(100.0)),
                        _ => None,
                    };
                }
            }
            if !devices.is_empty() {
                result.insert(process.pid, devices.into_values().collect());
            }
        }
        self.previous = next;
        result
    }
    pub async fn sample(
        &mut self,
        processes: &[ProcessUsage],
        nvidia: bool,
    ) -> (HashMap<u32, Vec<Usage>>, &'static str) {
        let mut result = self.drm(Path::new("/proc"), processes);
        let mut supported = !result.is_empty();
        if nvidia {
            let mut command = tokio::process::Command::new("nvidia-smi");
            command
                .args(["pmon", "-c", "1", "-s", "um"])
                .kill_on_drop(true);
            if let Ok(Ok(output)) =
                tokio::time::timeout(Duration::from_secs(3), command.output()).await
                && output.status.success()
                && let Some(rows) = parse_pmon(&String::from_utf8_lossy(&output.stdout))
            {
                supported = true;
                for process in processes {
                    if let Some(rows) = rows.get(&process.pid) {
                        result.entry(process.pid).or_default().extend(rows.clone());
                    }
                }
            }
        }
        // Recheck identity after driver queries: never attribute a recycled PID.
        result.retain(|pid, _| {
            processes
                .iter()
                .any(|p| p.pid == *pid && current_start(*pid) == Some(p.start_ticks))
        });
        (
            result,
            if supported {
                "available"
            } else {
                "unavailable"
            },
        )
    }
}
pub fn current_start(pid: u32) -> Option<u64> {
    let text =
        std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
    text[text.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drm_deduplicates_descriptors_and_computes_capacity_normalized_deltas() {
        let root =
            std::env::temp_dir().join(format!("tarsier-drm-{}-{}", std::process::id(), unix_ms()));
        let fd = root.join("42/fdinfo");
        std::fs::create_dir_all(&fd).unwrap();
        let info = "drm-driver: i915\ndrm-client-id: 2\ndrm-pdev: 0000:00:02.0\ndrm-engine-render: 2000000000 ns\ndrm-engine-capacity-render: 2\ndrm-resident-system: 12 KiB\n";
        std::fs::write(fd.join("3"), info).unwrap();
        std::fs::write(fd.join("4"), info).unwrap();
        let process = ProcessUsage {
            pid: 42,
            start_ticks: 100,
            name: "worker".into(),
            role: "Worker",
            cpu_percent: None,
            rss_bytes: 0,
            gpus: Vec::new(),
        };
        let mut collector = Collector::default();
        let first = collector.drm(&root, std::slice::from_ref(&process));
        assert_eq!(first[&42][0].memory_bytes, Some(12 * 1024));
        assert_eq!(first[&42][0].engines["render"], None);
        for value in collector.previous.values_mut() {
            *value = (1000000000, Instant::now() - Duration::from_secs(1));
        }
        let next = collector.drm(&root, std::slice::from_ref(&process));
        assert!((49.0..51.0).contains(&next[&42][0].engines["render"].unwrap()));
        let mut reused = process;
        reused.start_ticks += 1;
        assert_eq!(
            collector.drm(&root, &[reused])[&42][0].engines["render"],
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an NVIDIA GPU and the installed worker Python environment"]
    async fn live_nvidia_attributes_only_the_requested_cuda_process() {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new("worker/.venv/bin/python")
            .args(["-u", "-c", "import torch,time; x=torch.ones(1048576,device='cuda'); torch.cuda.synchronize(); print('ready',flush=True); time.sleep(20)"])
            .stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let pid = child.id().unwrap();
        let mut reader = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(15), reader.next_line())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line.as_deref(), Some("ready"));
        let process = ProcessUsage {
            pid,
            start_ticks: current_start(pid).unwrap(),
            name: "GPU test".into(),
            role: "Worker",
            cpu_percent: None,
            rss_bytes: 0,
            gpus: Vec::new(),
        };
        let (result, status) = Collector::default().sample(&[process], true).await;
        assert_eq!(status, "available");
        assert_eq!(result.len(), 1);
        assert!(
            result[&pid]
                .iter()
                .any(|gpu| gpu.source == "nvidia-smi pmon"
                    && gpu.memory_bytes.is_some_and(|bytes| bytes > 0))
        );
        println!(
            "Attributed test GPU usage: {}",
            serde_json::to_string(&result[&pid]).unwrap()
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[test]
    fn nvidia_columns_are_named_and_missing_values_stay_unknown() {
        let data=parse_pmon("# gpu pid type sm mem enc dec fb command\n0 42 C 25 8 - 0 120 python\n0 99 G - - - - - app\n").unwrap();
        assert_eq!(data[&42][0].engines["SM"], Some(25.0));
        assert_eq!(data[&42][0].engines["encode"], None);
        assert_eq!(data[&42][0].memory_bytes, Some(120 * 1048576));
        assert_eq!(data[&99][0].memory_bytes, None);
        assert!(parse_pmon("Not supported").is_none());
        assert!(
            parse_pmon("# gpu pid type sm fb command\n0 - - - - -\n")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn fdinfo_uses_resident_memory_once_and_reads_engine_capacity() {
        let client=parse_fdinfo("drm-driver: i915\ndrm-client-id: 2\ndrm-pdev: 0000:00:02.0\ndrm-engine-render: 100000000 ns\ndrm-engine-capacity-render: 2\ndrm-memory-system: 100 KiB\ndrm-resident-system: 12 KiB\ndrm-total-system: 100 KiB\n").unwrap();
        assert_eq!(client.memory, Some(12 * 1024));
        assert_eq!(client.engines["render"], 100000000);
        assert_eq!(client.capacities["render"], 2);
        assert!(parse_fdinfo("pos: 0\n").is_none());
    }
}
