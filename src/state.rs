use anyhow::{Context, Result};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Current status of a minidock container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerStatus {
    #[serde(alias = "Running")]
    Running,
    #[serde(alias = "Exited")]
    Exited,
    #[serde(alias = "Stopped")]
    Stopped,
}

impl std::fmt::Display for ContainerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Exited => write!(f, "exited"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Persisted state representation of a container.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerState {
    pub version: u8,
    pub id: Uuid,
    pub pid: i32,
    pub cgroup_path: PathBuf,
    pub rootfs: PathBuf,
    pub command: Vec<String>,
    pub hostname: String,
    pub detached: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: time::OffsetDateTime,
    pub status: ContainerStatus,
}

/// Checks if a process with the given host PID is alive.
///
/// Uses `kill(pid, 0)`:
/// - `ESRCH` means the process has exited (dead, returns false).
/// - `Ok(())` or `EPERM` means the process is alive (returns true).
/// - PIDs <= 0 or other errors return false.
pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => false,
    }
}

/// Filesystem-backed state store for minidock containers.
#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    /// Constructs a StateStore rooted at `$HOME/.minidock`.
    pub fn from_current_user() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME environment variable is not set")?;
        Ok(Self::at(PathBuf::from(home).join(".minidock")))
    }

    /// Constructs a StateStore rooted at the given directory.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Returns the root path of this StateStore.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the directory where JSON state files are stored (`<root>/state`).
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Returns the directory containing all container subdirectories (`<root>/containers`).
    pub fn containers_dir(&self) -> PathBuf {
        self.root.join("containers")
    }

    /// Returns the specific directory for a container (`<root>/containers/<id>`).
    pub fn container_dir(&self, id: Uuid) -> PathBuf {
        self.containers_dir().join(id.to_string())
    }

    /// Returns the rootfs directory path for a container (`<root>/containers/<id>/rootfs`).
    pub fn rootfs_dir(&self, id: Uuid) -> PathBuf {
        self.container_dir(id).join("rootfs")
    }

    /// Returns the rootfs directory path for a container (`<root>/containers/<id>/rootfs`).
    pub fn rootfs_path(&self, id: Uuid) -> PathBuf {
        self.rootfs_dir(id)
    }

    /// Returns the log file path for a container (`<root>/containers/<id>/container.log`).
    pub fn log_path(&self, id: Uuid) -> PathBuf {
        self.container_dir(id).join("container.log")
    }

    /// Creates the container rootfs directory (`<root>/containers/<id>/rootfs`)
    /// and its parent container directory, returning the rootfs path.
    pub fn create_container_dir(&self, id: Uuid) -> Result<PathBuf> {
        let rootfs = self.rootfs_dir(id);
        std::fs::create_dir_all(&rootfs).with_context(|| {
            format!(
                "failed to create container rootfs directory at {}",
                rootfs.display()
            )
        })?;
        Ok(rootfs)
    }

    /// Atomically saves the container state to disk.
    ///
    /// Writes JSON to `<state_dir>/<id>.json.tmp`, flushes via `sync_all`,
    /// and atomically renames the temporary file to `<state_dir>/<id>.json`.
    pub fn save(&self, state: &ContainerState) -> Result<()> {
        let state_dir = self.state_dir();
        std::fs::create_dir_all(&state_dir).with_context(|| {
            format!(
                "failed to create state directory at {}",
                state_dir.display()
            )
        })?;

        let target = state_dir.join(format!("{}.json", state.id));
        let temp = state_dir.join(format!("{}.json.tmp", state.id));

        let write_res = (|| -> Result<()> {
            let mut file = std::fs::File::create(&temp).with_context(|| {
                format!(
                    "failed to create temporary state file at {}",
                    temp.display()
                )
            })?;
            serde_json::to_writer_pretty(&mut file, state)
                .context("failed to serialize container state to JSON")?;
            file.sync_all().with_context(|| {
                format!("failed to sync temporary state file at {}", temp.display())
            })?;
            drop(file);

            std::fs::rename(&temp, &target).with_context(|| {
                format!(
                    "failed to rename {} to {}",
                    temp.display(),
                    target.display()
                )
            })?;
            Ok(())
        })();

        if write_res.is_err() && (temp.exists() || temp.is_symlink()) {
            let _ = std::fs::remove_file(&temp);
        }

        write_res
    }

    /// Loads and deserializes the container state from `<state_dir>/<id>.json`.
    pub fn load(&self, id: Uuid) -> Result<ContainerState> {
        let path = self.state_dir().join(format!("{id}.json"));
        let file = std::fs::File::open(&path).with_context(|| {
            format!("failed to open container state file at {}", path.display())
        })?;
        let state: ContainerState = serde_json::from_reader(file)
            .with_context(|| format!("failed to deserialize state from {}", path.display()))?;
        anyhow::ensure!(
            state.id == id,
            "container ID mismatch: requested {}, found {}",
            id,
            state.id
        );
        Ok(state)
    }

    /// Lists all containers in the state store, checking PID liveness for Running containers
    /// and sorting records ascending by `started_at`.
    pub fn list(&self) -> Result<Vec<ContainerState>> {
        let state_dir = self.state_dir();
        if !state_dir.exists() {
            return Ok(Vec::new());
        }

        let mut states = Vec::new();
        let entries = std::fs::read_dir(&state_dir).with_context(|| {
            format!("failed to read state directory at {}", state_dir.display())
        })?;

        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", state_dir.display()))?;
            let path = entry.path();

            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to get file type for {}", path.display()))?;
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            let file_id = Uuid::parse_str(stem).with_context(|| {
                format!("invalid container UUID in filename: {}", path.display())
            })?;

            let file = std::fs::File::open(&path)
                .with_context(|| format!("failed to open state file at {}", path.display()))?;
            let mut state: ContainerState = serde_json::from_reader(file)
                .with_context(|| format!("failed to deserialize state from {}", path.display()))?;

            anyhow::ensure!(
                state.id == file_id,
                "container ID mismatch in {}: filename has {}, state record has {}",
                path.display(),
                file_id,
                state.id
            );

            if state.status == ContainerStatus::Running && !is_pid_alive(state.pid) {
                state.status = ContainerStatus::Exited;
            }

            states.push(state);
        }

        states.sort_by_key(|state| state.started_at);
        Ok(states)
    }

    /// Updates the container status and saves the updated state.
    pub fn mark_status(&self, id: Uuid, status: ContainerStatus) -> Result<ContainerState> {
        let mut state = self.load(id)?;
        state.status = status;
        self.save(&state)?;
        Ok(state)
    }

    /// Opens the container's log file for reading.
    pub fn open_log(&self, id: Uuid) -> Result<std::fs::File> {
        let path = self.log_path(id);
        std::fs::File::open(&path)
            .with_context(|| format!("failed to open container log at {}", path.display()))
    }

    /// Creates or truncates the container's log file for writing.
    pub fn create_log(&self, id: Uuid) -> Result<std::fs::File> {
        let path = self.log_path(id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create directory for container log at {}",
                    parent.display()
                )
            })?;
        }
        std::fs::File::create(&path)
            .with_context(|| format!("failed to create container log at {}", path.display()))
    }

    /// Removes the state file and the container directory recursively.
    pub fn remove(&self, id: Uuid) -> Result<()> {
        let state_file = self.state_dir().join(format!("{id}.json"));
        if state_file.exists() || state_file.is_symlink() {
            std::fs::remove_file(&state_file).with_context(|| {
                format!("failed to remove state file at {}", state_file.display())
            })?;
        }
        let tmp_file = self.state_dir().join(format!("{id}.json.tmp"));
        if tmp_file.exists() || tmp_file.is_symlink() {
            let _ = std::fs::remove_file(&tmp_file);
        }
        let container_dir = self.container_dir(id);
        if container_dir.exists() || container_dir.is_symlink() {
            std::fs::remove_dir_all(&container_dir).with_context(|| {
                format!(
                    "failed to remove container directory at {}",
                    container_dir.display()
                )
            })?;
        }
        Ok(())
    }
}
