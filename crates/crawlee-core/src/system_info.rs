//! CPU and memory measurements behind the `SystemInfo` event, ported from
//! `@crawlee/core/system-info`.
//!
//! - **Outside a container:** the machine's CPU usage and total memory.
//! - **In a container:** the cgroup (v1 or v2) CPU quota and memory limit. Autoscaling then
//!   respects the limits of the container rather than of the host.
//!
//! Memory used is the resident set size of this process plus all its descendants (such as
//! browsers).

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// One measurement, the payload of [`Event::SystemInfo`](crate::events::Event::SystemInfo).
#[derive(Clone, Debug, PartialEq)]
pub struct SystemInfo {
    pub created_at: DateTime<Utc>,
    /// CPU usage in percent of what the process may use (all cores, or the cgroup quota).
    pub cpu_current_usage: f64,
    /// Usage above [`Configuration::max_used_cpu_ratio`](crate::Configuration::max_used_cpu_ratio).
    pub is_cpu_overloaded: bool,
    /// Memory available to the process: the container limit, or the machine's memory.
    pub mem_total_bytes: Option<u64>,
    /// Resident memory of this process and its descendants.
    pub mem_current_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CgroupVersion {
    V1,
    V2,
}

fn cgroup_version() -> Option<CgroupVersion> {
    if !Path::new("/sys/fs/cgroup/").exists() {
        return None;
    }
    Some(if Path::new("/sys/fs/cgroup/memory/").exists() { CgroupVersion::V1 } else { CgroupVersion::V2 })
}

fn is_lambda() -> bool {
    std::env::var_os("AWS_LAMBDA_FUNCTION_MEMORY_SIZE").is_some()
}

/// Whether the process runs in Docker or Kubernetes, detected like Crawlee for JS does.
pub fn is_containerized() -> bool {
    if is_lambda() {
        return false;
    }
    Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|content| content.contains("docker"))
        || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
}

fn read_trimmed(path: &str) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

/// `(limit, used)` of the cgroup, or `None` when cgroups are not readable. An unlimited
/// cgroup reports `None` as the limit.
fn cgroup_memory(version: CgroupVersion) -> std::io::Result<(Option<u64>, u64)> {
    let (limit_path, used_path) = match version {
        CgroupVersion::V1 => {
            ("/sys/fs/cgroup/memory/memory.limit_in_bytes", "/sys/fs/cgroup/memory/memory.usage_in_bytes")
        }
        CgroupVersion::V2 => ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.current"),
    };
    let limit = read_trimmed(limit_path)?;
    let used = read_trimmed(used_path)?.parse().map_err(std::io::Error::other)?;
    // v2 writes `max`; v1 writes a huge page-aligned number when unlimited.
    let limit = match limit.as_str() {
        "max" => None,
        value => value.parse::<u64>().ok().filter(|&bytes| bytes <= (1 << 53)),
    };
    Ok((limit, used))
}

/// CPU quota as the number of cores the cgroup may use; `None` when unlimited.
fn cgroup_cpu_allowance(version: CgroupVersion) -> std::io::Result<Option<f64>> {
    match version {
        CgroupVersion::V1 => {
            let quota: i64 =
                read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")?.parse().map_err(std::io::Error::other)?;
            if quota == -1 {
                return Ok(None);
            }
            let period: f64 =
                read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_period_us")?.parse().map_err(std::io::Error::other)?;
            Ok(Some(quota as f64 / period))
        }
        CgroupVersion::V2 => {
            let max = read_trimmed("/sys/fs/cgroup/cpu.max")?;
            let mut parts = max.split_whitespace();
            let quota = parts.next().unwrap_or("max");
            if quota == "max" {
                return Ok(None);
            }
            let quota: f64 = quota.parse().map_err(std::io::Error::other)?;
            let period: f64 = parts.next().unwrap_or("100000").parse().map_err(std::io::Error::other)?;
            Ok(Some(quota / period))
        }
    }
}

/// CPU time used by the cgroup, in nanoseconds.
fn cgroup_cpu_usage(version: CgroupVersion) -> std::io::Result<f64> {
    match version {
        CgroupVersion::V1 => {
            read_trimmed("/sys/fs/cgroup/cpuacct/cpuacct.usage")?.parse().map_err(std::io::Error::other)
        }
        CgroupVersion::V2 => {
            let stat = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat")?;
            let usec = stat
                .lines()
                .find_map(|line| line.strip_prefix("usage_usec "))
                .and_then(|value| value.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            Ok(usec * 1000.0)
        }
    }
}

/// CPU time of the whole system (user through softirq), in nanoseconds.
fn system_cpu_usage(clock_ticks_per_second: f64) -> std::io::Result<f64> {
    let stat = std::fs::read_to_string("/proc/stat")?;
    let line = stat
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| std::io::Error::other("no cpu line in /proc/stat"))?;
    let ticks: f64 = line.split_whitespace().skip(1).take(7).filter_map(|v| v.parse::<f64>().ok()).sum();
    Ok(ticks * 1e9 / clock_ticks_per_second)
}

fn clock_ticks_per_second() -> f64 {
    std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(100.0)
}

#[derive(Default)]
struct CgroupCpuSample {
    container: f64,
    system: f64,
}

/// Takes [`SystemInfo`] measurements. CPU usage is measured between two calls, so the sampler
/// keeps the previous sample.
pub struct SystemInfoSampler {
    system: System,
    pid: Option<Pid>,
    containerized: bool,
    max_used_cpu_ratio: f64,
    clock_ticks_per_second: Option<f64>,
    previous_cgroup_sample: CgroupCpuSample,
    warned: HashSet<&'static str>,
}

impl std::fmt::Debug for SystemInfoSampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemInfoSampler").field("containerized", &self.containerized).finish_non_exhaustive()
    }
}

impl SystemInfoSampler {
    /// `containerized: None` detects it with [`is_containerized`].
    pub fn new(containerized: Option<bool>, max_used_cpu_ratio: f64) -> Self {
        SystemInfoSampler {
            system: System::new(),
            pid: sysinfo::get_current_pid().ok(),
            containerized: containerized.unwrap_or_else(is_containerized),
            max_used_cpu_ratio,
            clock_ticks_per_second: None,
            previous_cgroup_sample: CgroupCpuSample::default(),
            warned: HashSet::new(),
        }
    }

    fn warn_once(&mut self, key: &'static str, message: &str) {
        if self.warned.insert(key) {
            tracing::warn!("{message}");
        }
    }

    pub fn sample(&mut self) -> SystemInfo {
        let cpu_ratio = self.cpu_usage_ratio();
        let (mem_total_bytes, mem_current_bytes) = self.memory();
        SystemInfo {
            created_at: Utc::now(),
            cpu_current_usage: cpu_ratio * 100.0,
            is_cpu_overloaded: cpu_ratio > self.max_used_cpu_ratio,
            mem_total_bytes,
            mem_current_bytes,
        }
    }

    fn host_cpu_ratio(&mut self) -> f64 {
        self.system.refresh_cpu_usage();
        f64::from(self.system.global_cpu_usage()) / 100.0
    }

    fn cpu_usage_ratio(&mut self) -> f64 {
        if !self.containerized {
            return self.host_cpu_ratio();
        }
        let Some(version) = cgroup_version() else {
            self.warn_once(
                "cpu-cgroup",
                "Your environment is containerized, but your system does not support cgroups. If you're running \
                 containers with limited CPU, CPU autoscaling will not work properly.",
            );
            return self.host_cpu_ratio();
        };
        match self.cgroup_cpu_ratio(version) {
            Ok(Some(ratio)) => ratio,
            // No quota: the host's load is the one that matters.
            Ok(None) => self.host_cpu_ratio(),
            Err(err) => {
                self.warn_once(
                    "cpu-failed",
                    &format!("CPU snapshot failed, falling back to bare-metal metrics: {err}"),
                );
                self.host_cpu_ratio()
            }
        }
    }

    fn cgroup_cpu_ratio(&mut self, version: CgroupVersion) -> std::io::Result<Option<f64>> {
        let Some(allowance) = cgroup_cpu_allowance(version)? else {
            return Ok(None);
        };
        let ticks = *self.clock_ticks_per_second.get_or_insert_with(clock_ticks_per_second);
        let sample = CgroupCpuSample { container: cgroup_cpu_usage(version)?, system: system_cpu_usage(ticks)? };
        let container_delta = sample.container - self.previous_cgroup_sample.container;
        let system_delta = sample.system - self.previous_cgroup_sample.system;
        self.previous_cgroup_sample = sample;
        if system_delta <= 0.0 {
            return Ok(Some(0.0));
        }
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        Ok(Some(container_delta / system_delta * cores / allowance))
    }

    /// Resident memory of this process and all its descendants.
    fn process_tree_memory(&mut self) -> Option<u64> {
        let pid = self.pid?;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let processes = self.system.processes();
        let mut tree: HashSet<Pid> = HashSet::from([pid]);
        // Parents come before children only by chance, so repeat until no process joins the tree.
        loop {
            let before = tree.len();
            for (child, process) in processes {
                if process.parent().is_some_and(|parent| tree.contains(&parent)) {
                    tree.insert(*child);
                }
            }
            if tree.len() == before {
                break;
            }
        }
        Some(tree.iter().filter_map(|pid| processes.get(pid)).map(|process| process.memory()).sum())
    }

    fn memory(&mut self) -> (Option<u64>, Option<u64>) {
        let used = self.process_tree_memory();
        self.system.refresh_memory();
        let host_total = self.system.total_memory();

        if is_lambda() {
            let total = std::env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE")
                .ok()
                .and_then(|mb| mb.parse::<u64>().ok())
                .map(|mb| mb * 1_000_000);
            return (total, used);
        }
        if self.containerized {
            match cgroup_version().map(cgroup_memory) {
                Some(Ok((limit, _))) => return (Some(limit.unwrap_or(host_total)), used),
                Some(Err(err)) => self.warn_once(
                    "memory-cgroup",
                    &format!(
                        "Your environment is containerized, but your system does not support memory cgroups. If \
                         you're running containers with limited memory, memory autoscaling will not work properly. \
                         Cause: {err}"
                    ),
                ),
                None => self.warn_once(
                    "memory-cgroup",
                    "Your environment is containerized, but your system does not support memory cgroups. If you're \
                     running containers with limited memory, memory autoscaling will not work properly.",
                ),
            }
        }
        (Some(host_total), used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_this_process() {
        let mut sampler = SystemInfoSampler::new(Some(false), 0.95);
        let first = sampler.sample();
        let second = sampler.sample();
        let used = second.mem_current_bytes.expect("the process memory is readable");
        assert!(used > 1 << 20, "a test process uses more than 1 MiB: {used}");
        assert!(second.mem_total_bytes.unwrap() > used);
        for info in [first, second] {
            assert!((0.0..=100.0).contains(&info.cpu_current_usage), "{info:?}");
        }
    }
}
