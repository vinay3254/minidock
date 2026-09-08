//! Cgroups management module.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// User-configured resource limits for a container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: Option<u64>,
    pub cpu_percent: Option<u8>,
}

/// Container cgroup paths for active controllers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerCgroups {
    pub memory: Option<PathBuf>,
    pub cpu: Option<PathBuf>,
}

impl ContainerCgroups {
    /// Attaches the given host PID to the cgroup hierarchy for all active controllers.
    pub fn attach(&self, host_pid: i32) -> anyhow::Result<()> {
        for path in [self.memory.as_ref(), self.cpu.as_ref()]
            .into_iter()
            .flatten()
        {
            let tasks = path.join("tasks");
            std::fs::write(&tasks, format!("{host_pid}\n")).with_context(|| {
                format!(
                    "failed to attach PID {host_pid} to cgroup tasks file {}",
                    tasks.display()
                )
            })?;
        }
        Ok(())
    }

    /// Removes empty container cgroup directories if they exist.
    pub fn remove_empty(&self) -> anyhow::Result<()> {
        for path in [self.memory.as_ref(), self.cpu.as_ref()]
            .into_iter()
            .flatten()
        {
            if path.exists() {
                std::fs::remove_dir(path).with_context(|| {
                    format!("failed to remove cgroup directory {}", path.display())
                })?;
            }
        }
        Ok(())
    }
}

/// Discovers and manages cgroup v1 controllers on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupManager {
    pub memory_mount: Option<PathBuf>,
    pub cpu_mount: Option<PathBuf>,
}

impl CgroupManager {
    /// Creates a new `CgroupManager` with explicit controller mount paths.
    pub fn new(memory_mount: Option<PathBuf>, cpu_mount: Option<PathBuf>) -> Self {
        Self {
            memory_mount,
            cpu_mount,
        }
    }

    /// Detects cgroup v1 controllers from the host `/proc` files.
    pub fn detect() -> anyhow::Result<Self> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .context("failed to read /proc/self/mountinfo")?;
        let self_cgroup = std::fs::read_to_string("/proc/self/cgroup")
            .context("failed to read /proc/self/cgroup")?;
        Self::discover_from(&mountinfo, &self_cgroup)
    }

    /// Discovers cgroup v1 controller paths from `mountinfo` and `self_cgroup` content strings.
    pub fn discover_from(mountinfo: &str, self_cgroup: &str) -> anyhow::Result<Self> {
        let mut has_cgroup2 = false;
        let mut has_cgroup_v1 = false;
        let mut memory_mount_point: Option<PathBuf> = None;
        let mut cpu_mount_point: Option<PathBuf> = None;

        for line in mountinfo.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (pre, post) = match line.split_once(" - ") {
                Some(pair) => pair,
                None => continue,
            };
            let pre_fields: Vec<&str> = pre.split_whitespace().collect();
            let post_fields: Vec<&str> = post.split_whitespace().collect();
            if pre_fields.len() < 5 || post_fields.is_empty() {
                continue;
            }
            let mount_point = pre_fields[4];
            let fstype = post_fields[0];

            if fstype == "cgroup2" {
                has_cgroup2 = true;
                continue;
            }

            if fstype == "cgroup" {
                has_cgroup_v1 = true;
                let super_options = post_fields.get(2).copied().unwrap_or("");
                let mount_options = pre_fields.get(5).copied().unwrap_or("");

                let all_options: Vec<&str> = super_options
                    .split(',')
                    .chain(mount_options.split(','))
                    .collect();

                let has_memory_opt = all_options.contains(&"memory");
                let has_cpu_opt =
                    all_options.contains(&"cpu") || all_options.contains(&"cpu,cpuacct");

                let mp_path = Path::new(mount_point);
                let file_name = mp_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let is_memory = has_memory_opt || file_name == "memory";
                let is_cpu = has_cpu_opt || file_name == "cpu" || file_name == "cpu,cpuacct";

                if is_memory && memory_mount_point.is_none() {
                    memory_mount_point = Some(PathBuf::from(mount_point));
                }
                if is_cpu && cpu_mount_point.is_none() {
                    cpu_mount_point = Some(PathBuf::from(mount_point));
                }
            }
        }

        if !has_cgroup_v1 {
            if has_cgroup2 {
                anyhow::bail!(
                    "cgroups v2-only hosts are not supported; cgroups v1 memory and cpu controllers required"
                );
            } else {
                anyhow::bail!(
                    "no cgroups v1 mounts found; cgroups v1 memory and cpu controllers required"
                );
            }
        }

        let memory_mount = memory_mount_point.map(|mp| {
            let self_path = find_subsystem_path(self_cgroup, "memory");
            combine_mount_and_cgroup_path(mp, self_path.as_deref())
        });

        let cpu_mount = cpu_mount_point.map(|mp| {
            let self_path = find_subsystem_path(self_cgroup, "cpu");
            combine_mount_and_cgroup_path(mp, self_path.as_deref())
        });

        Ok(Self {
            memory_mount,
            cpu_mount,
        })
    }

    /// Creates container cgroup directories under active controllers and configures limits.
    pub fn create(&self, id: uuid::Uuid, limits: Limits) -> anyhow::Result<ContainerCgroups> {
        let memory = if let Some(bytes) = limits.memory_bytes {
            let base = self.memory_mount.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "memory limit requested but memory cgroup controller is not available"
                )
            })?;
            let cgroup_dir = base.join("minidock").join(id.to_string());
            std::fs::create_dir_all(&cgroup_dir).with_context(|| {
                format!(
                    "failed to create memory cgroup directory at {}",
                    cgroup_dir.display()
                )
            })?;
            let limit_file = cgroup_dir.join("memory.limit_in_bytes");
            if let Err(e) = std::fs::write(&limit_file, format!("{bytes}\n")) {
                let _ = std::fs::remove_dir_all(&cgroup_dir);
                return Err(e)
                    .with_context(|| format!("failed to write limit to {}", limit_file.display()));
            }
            Some(cgroup_dir)
        } else {
            None
        };

        let cpu = if let Some(percent) = limits.cpu_percent {
            let base = match self.cpu_mount.as_ref() {
                Some(b) => b,
                None => {
                    if let Some(ref mem_dir) = memory {
                        let _ = std::fs::remove_dir_all(mem_dir);
                    }
                    anyhow::bail!("cpu limit requested but cpu cgroup controller is not available");
                }
            };
            let cgroup_dir = base.join("minidock").join(id.to_string());
            if let Err(e) = std::fs::create_dir_all(&cgroup_dir) {
                if let Some(ref mem_dir) = memory {
                    let _ = std::fs::remove_dir_all(mem_dir);
                }
                return Err(e).with_context(|| {
                    format!(
                        "failed to create cpu cgroup directory at {}",
                        cgroup_dir.display()
                    )
                });
            }

            let period = match read_period(&cgroup_dir, base) {
                Ok(p) => p,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&cgroup_dir);
                    if let Some(ref mem_dir) = memory {
                        let _ = std::fs::remove_dir_all(mem_dir);
                    }
                    return Err(e);
                }
            };

            let quota = cpu_quota(period, percent);
            let quota_file = cgroup_dir.join("cpu.cfs_quota_us");
            if let Err(e) = std::fs::write(&quota_file, format!("{quota}\n")) {
                let _ = std::fs::remove_dir_all(&cgroup_dir);
                if let Some(ref mem_dir) = memory {
                    let _ = std::fs::remove_dir_all(mem_dir);
                }
                return Err(e).with_context(|| {
                    format!("failed to write cpu quota to {}", quota_file.display())
                });
            }
            Some(cgroup_dir)
        } else {
            None
        };

        Ok(ContainerCgroups { memory, cpu })
    }
}

fn read_period(cgroup_dir: &Path, base_dir: &Path) -> anyhow::Result<u64> {
    let cgroup_period_file = cgroup_dir.join("cpu.cfs_period_us");
    match std::fs::read_to_string(&cgroup_period_file) {
        Ok(s) => s
            .trim()
            .parse::<u64>()
            .with_context(|| format!("failed to parse {}", cgroup_period_file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let base_period_file = base_dir.join("cpu.cfs_period_us");
            match std::fs::read_to_string(&base_period_file) {
                Ok(s) => s
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("failed to parse {}", base_period_file.display())),
                Err(be) if be.kind() == std::io::ErrorKind::NotFound => Ok(100_000),
                Err(be) => Err(be)
                    .with_context(|| format!("failed to read {}", base_period_file.display())),
            }
        }
        Err(e) => {
            Err(e).with_context(|| format!("failed to read {}", cgroup_period_file.display()))
        }
    }
}

fn find_subsystem_path(self_cgroup: &str, target_subsys: &str) -> Option<String> {
    for line in self_cgroup.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() == 3 {
            let subsys_list = parts[1];
            let path = parts[2];
            let matches = subsys_list.split(',').any(|s| s == target_subsys);
            if matches {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn combine_mount_and_cgroup_path(mount: PathBuf, cgroup_path: Option<&str>) -> PathBuf {
    match cgroup_path {
        Some(p) => {
            let clean = p.trim().trim_start_matches('/');
            if clean.is_empty() {
                mount
            } else {
                mount.join(clean)
            }
        }
        None => mount,
    }
}

/// Parses a human-readable memory limit string into bytes.
///
/// Supports plain byte integers, or suffixes `b`/`B` (1), `k`/`K` (1024),
/// `m`/`M` (1024^2), `g`/`G` (1024^3), and `t`/`T` (1024^4).
/// Rejects 0, negative numbers, invalid suffixes (e.g. `12MB`), empty strings, and overflow.
pub fn parse_memory_limit(s: &str) -> anyhow::Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("memory limit cannot be empty");
    }
    if trimmed.starts_with('-') {
        anyhow::bail!("memory limit cannot be negative");
    }

    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());

    let (num_str, suffix) = trimmed.split_at(digit_end);
    if num_str.is_empty() {
        anyhow::bail!("invalid memory limit '{s}': missing numeric value");
    }

    let value = num_str
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("failed to parse memory limit '{s}': {e}"))?;

    if value == 0 {
        anyhow::bail!("memory limit must be greater than 0");
    }

    let multiplier: u64 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        "t" => 1024 * 1024 * 1024 * 1024,
        _ => anyhow::bail!("invalid memory limit suffix in '{s}'"),
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("memory limit '{s}' overflows u64"))
}

/// Calculates the CFS quota in microseconds given period and desired CPU percentage.
///
/// Ensures the quota honors the kernel minimum of 1000 microseconds (1 ms).
pub fn cpu_quota(period: u64, percent: u8) -> u64 {
    std::cmp::max(1000, period.saturating_mul(percent as u64) / 100)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const V2_ONLY_MOUNTINFO: &str = "\
46 44 0:29 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:7 - cgroup2 cgroup2 rw,nsdelegate\n";

    const SELF_CGROUP_V2: &str = "\
0::/user.slice/user-1000.slice/user@1000.service/app.slice\n";

    const V1_VALID_MOUNTINFO: &str = "\
30 26 0:25 / /sys/fs/cgroup/memory rw,nosuid,nodev,noexec,relatime shared:12 - cgroup cgroup rw,memory\n\
31 26 0:26 / /sys/fs/cgroup/cpu rw,nosuid,nodev,noexec,relatime shared:13 - cgroup cgroup rw,cpu\n";

    const V1_CO_MOUNTED_MOUNTINFO: &str = "\
30 26 0:25 / /sys/fs/cgroup/memory rw,nosuid,nodev,noexec,relatime shared:12 - cgroup cgroup rw,memory\n\
31 26 0:26 / /sys/fs/cgroup/cpu,cpuacct rw,nosuid,nodev,noexec,relatime shared:13 - cgroup cgroup rw,cpu,cpuacct\n";

    const SELF_CGROUP_V1_ROOT: &str = "\
10:memory:/\n\
9:cpu,cpuacct:/\n";

    const SELF_CGROUP_V1_NESTED: &str = "\
10:memory:/docker/abcdef123456\n\
9:cpu,cpuacct:/docker/abcdef123456\n";

    #[test]
    fn parses_binary_memory_suffixes() {
        assert_eq!(parse_memory_limit("128M").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_memory_limit("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_limit("64k").unwrap(), 64 * 1024);
        assert_eq!(parse_memory_limit("64K").unwrap(), 64 * 1024);
        assert_eq!(parse_memory_limit("1t").unwrap(), 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_limit("1T").unwrap(), 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_limit("1024").unwrap(), 1024);
        assert_eq!(parse_memory_limit("1024b").unwrap(), 1024);
        assert_eq!(parse_memory_limit("1024B").unwrap(), 1024);

        assert!(parse_memory_limit("12MB").is_err());
        assert!(parse_memory_limit("0").is_err());
        assert!(parse_memory_limit("0M").is_err());
        assert!(parse_memory_limit("-1").is_err());
        assert!(parse_memory_limit("").is_err());
        assert!(parse_memory_limit("   ").is_err());
        assert!(parse_memory_limit("abc").is_err());
        assert!(parse_memory_limit("18446744073709551615M").is_err());
    }

    #[test]
    fn quota_honors_the_kernel_minimum() {
        assert_eq!(cpu_quota(100_000, 50), 50_000);
        assert_eq!(cpu_quota(100_000, 1), 1_000);
        assert_eq!(cpu_quota(100_000, 0), 1_000);
        assert_eq!(cpu_quota(100_000, 100), 100_000);
        assert_eq!(cpu_quota(500, 50), 1_000);
    }

    #[test]
    fn unified_only_mountinfo_is_rejected() {
        assert!(CgroupManager::discover_from(V2_ONLY_MOUNTINFO, SELF_CGROUP_V2).is_err());
    }

    #[test]
    fn discovers_separate_v1_controllers() {
        let mgr = CgroupManager::discover_from(V1_VALID_MOUNTINFO, SELF_CGROUP_V1_ROOT).unwrap();
        assert_eq!(
            mgr.memory_mount,
            Some(PathBuf::from("/sys/fs/cgroup/memory"))
        );
        assert_eq!(mgr.cpu_mount, Some(PathBuf::from("/sys/fs/cgroup/cpu")));
    }

    #[test]
    fn discovers_co_mounted_cpu_cpuacct() {
        let mgr =
            CgroupManager::discover_from(V1_CO_MOUNTED_MOUNTINFO, SELF_CGROUP_V1_ROOT).unwrap();
        assert_eq!(
            mgr.memory_mount,
            Some(PathBuf::from("/sys/fs/cgroup/memory"))
        );
        assert_eq!(
            mgr.cpu_mount,
            Some(PathBuf::from("/sys/fs/cgroup/cpu,cpuacct"))
        );
    }

    #[test]
    fn combines_nested_self_cgroup_path() {
        let mgr =
            CgroupManager::discover_from(V1_CO_MOUNTED_MOUNTINFO, SELF_CGROUP_V1_NESTED).unwrap();
        assert_eq!(
            mgr.memory_mount,
            Some(PathBuf::from("/sys/fs/cgroup/memory/docker/abcdef123456"))
        );
        assert_eq!(
            mgr.cpu_mount,
            Some(PathBuf::from(
                "/sys/fs/cgroup/cpu,cpuacct/docker/abcdef123456"
            ))
        );
    }

    #[test]
    fn mock_create_attach_and_remove_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mem_base = tmp.path().join("memory");
        let cpu_base = tmp.path().join("cpu");
        std::fs::create_dir_all(&mem_base).unwrap();
        std::fs::create_dir_all(&cpu_base).unwrap();

        let mgr = CgroupManager {
            memory_mount: Some(mem_base.clone()),
            cpu_mount: Some(cpu_base.clone()),
        };

        let id = uuid::Uuid::new_v4();
        let limits = Limits {
            memory_bytes: Some(64 * 1024 * 1024),
            cpu_percent: Some(50),
        };

        let cgroups = mgr.create(id, limits).unwrap();
        let mem_cg = cgroups.memory.as_ref().unwrap();
        let cpu_cg = cgroups.cpu.as_ref().unwrap();

        let mem_limit_file = mem_cg.join("memory.limit_in_bytes");
        let cpu_quota_file = cpu_cg.join("cpu.cfs_quota_us");
        assert!(mem_limit_file.exists());
        assert!(cpu_quota_file.exists());

        assert_eq!(
            std::fs::read_to_string(&mem_limit_file).unwrap().trim(),
            "67108864"
        );
        assert_eq!(
            std::fs::read_to_string(&cpu_quota_file).unwrap().trim(),
            "50000"
        );

        // Test attach
        cgroups.attach(12345).unwrap();
        assert_eq!(
            std::fs::read_to_string(mem_cg.join("tasks"))
                .unwrap()
                .trim(),
            "12345"
        );
        assert_eq!(
            std::fs::read_to_string(cpu_cg.join("tasks"))
                .unwrap()
                .trim(),
            "12345"
        );

        // Clean up files so remove_empty can succeed
        std::fs::remove_file(mem_cg.join("tasks")).unwrap();
        std::fs::remove_file(mem_limit_file).unwrap();
        std::fs::remove_file(cpu_cg.join("tasks")).unwrap();
        std::fs::remove_file(cpu_quota_file).unwrap();

        cgroups.remove_empty().unwrap();
        assert!(!mem_cg.exists());
        assert!(!cpu_cg.exists());
    }

    #[test]
    fn mock_create_reads_period_from_base() {
        let tmp = tempfile::tempdir().unwrap();
        let cpu_base = tmp.path().join("cpu");
        std::fs::create_dir_all(&cpu_base).unwrap();
        std::fs::write(cpu_base.join("cpu.cfs_period_us"), "200000\n").unwrap();

        let mgr = CgroupManager {
            memory_mount: None,
            cpu_mount: Some(cpu_base.clone()),
        };

        let id = uuid::Uuid::new_v4();
        let limits = Limits {
            memory_bytes: None,
            cpu_percent: Some(25),
        };

        let cgroups = mgr.create(id, limits).unwrap();
        assert!(cgroups.memory.is_none());
        let cpu_cg = cgroups.cpu.as_ref().unwrap();

        let cpu_quota_file = cpu_cg.join("cpu.cfs_quota_us");
        assert_eq!(
            std::fs::read_to_string(&cpu_quota_file).unwrap().trim(),
            "50000"
        );
    }

    #[test]
    fn mock_create_fails_when_controller_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let mem_base = tmp.path().join("memory");
        std::fs::create_dir_all(&mem_base).unwrap();

        let mgr = CgroupManager {
            memory_mount: Some(mem_base),
            cpu_mount: None,
        };

        let id = uuid::Uuid::new_v4();
        let limits = Limits {
            memory_bytes: Some(1024),
            cpu_percent: Some(50),
        };

        assert!(mgr.create(id, limits).is_err());
    }

    #[test]
    fn no_cgroups_mountinfo_is_rejected() {
        const EXT4_ONLY_MOUNTINFO: &str = "\
41 1 259:5 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p5 rw\n";
        assert!(CgroupManager::discover_from(EXT4_ONLY_MOUNTINFO, SELF_CGROUP_V1_ROOT).is_err());
    }

    #[test]
    fn empty_limits_creates_no_cgroups_and_attaches_cleanly() {
        let mgr = CgroupManager {
            memory_mount: Some(PathBuf::from("/mock/memory")),
            cpu_mount: Some(PathBuf::from("/mock/cpu")),
        };
        let cgroups = mgr.create(uuid::Uuid::new_v4(), Limits::default()).unwrap();
        assert_eq!(cgroups.memory, None);
        assert_eq!(cgroups.cpu, None);

        // attach and remove_empty should be no-ops and succeed
        assert!(cgroups.attach(1234).is_ok());
        assert!(cgroups.remove_empty().is_ok());
    }

    #[test]
    fn invalid_cpu_period_content_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let cpu_base = tmp.path().join("cpu");
        std::fs::create_dir_all(&cpu_base).unwrap();
        std::fs::write(cpu_base.join("cpu.cfs_period_us"), "not_a_number\n").unwrap();

        let mgr = CgroupManager {
            memory_mount: None,
            cpu_mount: Some(cpu_base),
        };
        let limits = Limits {
            memory_bytes: None,
            cpu_percent: Some(50),
        };
        assert!(mgr.create(uuid::Uuid::new_v4(), limits).is_err());
    }

    #[test]
    fn remove_empty_succeeds_when_paths_do_not_exist() {
        let cgroups = ContainerCgroups {
            memory: Some(PathBuf::from("/nonexistent/path/mem")),
            cpu: Some(PathBuf::from("/nonexistent/path/cpu")),
        };
        assert!(cgroups.remove_empty().is_ok());
    }
}
