use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;
use uuid::Uuid;

use crate::cgroups::{CgroupManager, Limits};
use crate::image::extract_rootfs;
use crate::state::{is_pid_alive, ContainerState, ContainerStatus, StateStore};

static FORWARD_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn signal_handler(sig: libc::c_int) {
    FORWARD_SIGNAL.store(sig, Ordering::SeqCst);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub image: PathBuf,
    pub memory_bytes: Option<u64>,
    pub cpu_percent: Option<u8>,
    pub hostname: String,
    pub detached: bool,
    pub command: Vec<String>,
}

#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct InitArgs {
    #[arg(long)]
    pub rootfs: Option<PathBuf>,
    #[arg(long)]
    pub hostname: Option<String>,
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StatusMessage {
    InitPid(i32),
    Ready,
    Error(String),
    Exit(i32),
}

impl StatusMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes =
            serde_json::to_vec(self).expect("status message serialization should never fail");
        bytes.push(b'\n');
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("invalid status message: {}", e))
    }
}

pub fn exit_code_from_wait_status(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, sig, _) => 128 + sig as i32,
        _ => 1,
    }
}

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("root privileges are required to run containers");
    }
    Ok(())
}

pub trait ProcessOps {
    fn kill(&mut self, pid: Pid, signal: Signal) -> Result<()>;
    fn is_alive(&mut self, pid: Pid) -> bool;
    fn sleep(&mut self, duration: Duration);
}

pub struct RealProcessOps;

impl ProcessOps for RealProcessOps {
    fn kill(&mut self, pid: Pid, signal: Signal) -> Result<()> {
        match nix::sys::signal::kill(pid, signal) {
            Ok(()) => Ok(()),
            Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "failed to send signal {:?} to pid {}: {}",
                signal,
                pid,
                e
            )),
        }
    }

    fn is_alive(&mut self, pid: Pid) -> bool {
        is_pid_alive(pid.as_raw())
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub struct RecordingProcessOps<'a> {
    pub signals: &'a mut Vec<Signal>,
    pub alive: bool,
}

impl<'a> RecordingProcessOps<'a> {
    pub fn new(signals: &'a mut Vec<Signal>) -> Self {
        Self {
            signals,
            alive: true,
        }
    }
}

impl<'a> ProcessOps for RecordingProcessOps<'a> {
    fn kill(&mut self, _pid: Pid, signal: Signal) -> Result<()> {
        self.signals.push(signal);
        Ok(())
    }

    fn is_alive(&mut self, _pid: Pid) -> bool {
        self.alive
    }

    fn sleep(&mut self, _duration: Duration) {}
}

pub fn stop_with<O: ProcessOps>(ops: &mut O, pid: Pid) -> Result<()> {
    ops.kill(pid, Signal::SIGTERM)?;
    for _ in 0..10 {
        if !ops.is_alive(pid) {
            return Ok(());
        }
        ops.sleep(Duration::from_secs(1));
    }
    if ops.is_alive(pid) {
        ops.kill(pid, Signal::SIGKILL)?;
    }
    Ok(())
}

pub fn stop(id: Uuid, store: &StateStore) -> Result<()> {
    let state = store.load(id)?;
    if state.status == ContainerStatus::Running {
        let pid = Pid::from_raw(state.pid);
        let mut ops = RealProcessOps;
        stop_with(&mut ops, pid)?;
    }
    store.mark_status(id, ContainerStatus::Stopped)?;
    Ok(())
}

fn write_status_message<W: Write>(writer: &mut W, msg: &StatusMessage) -> Result<()> {
    let bytes = msg.encode();
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_status_message<R: BufRead>(reader: &mut R) -> Result<StatusMessage> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("status pipe closed unexpectedly");
    }
    StatusMessage::decode(line.trim_end().as_bytes())
}

fn setup_isolated_filesystem(rootfs: &Path) -> Result<()> {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};

    let abs_rootfs = std::fs::canonicalize(rootfs)
        .with_context(|| format!("failed to canonicalize rootfs path {}", rootfs.display()))?;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("failed to make / recursively private")?;

    mount(
        Some(&abs_rootfs),
        &abs_rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("failed to bind-mount rootfs at {}", abs_rootfs.display()))?;

    let container_dev = abs_rootfs.join("dev");
    let _ = std::fs::create_dir_all(&container_dev);
    let _ = mount(
        Some("/dev"),
        &container_dev,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    );

    let old_root = abs_rootfs.join(".old_root");
    std::fs::create_dir_all(&old_root)
        .with_context(|| format!("failed to create old_root at {}", old_root.display()))?;

    let c_rootfs = CString::new(abs_rootfs.to_str().context("invalid rootfs path")?)?;
    let c_old_root = CString::new(old_root.to_str().context("invalid old_root path")?)?;
    let ret =
        unsafe { libc::syscall(libc::SYS_pivot_root, c_rootfs.as_ptr(), c_old_root.as_ptr()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error()).context("pivot_root syscall failed");
    }

    nix::unistd::chdir("/").context("failed to chdir to / after pivot_root")?;

    umount2("/.old_root", MntFlags::MNT_DETACH).context("failed to unmount /.old_root")?;
    let _ = std::fs::remove_dir("/.old_root");

    std::fs::create_dir_all("/proc").ok();
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .context("failed to mount /proc")?;

    Ok(())
}

fn setup_hostname(hostname: &str) -> Result<()> {
    nix::unistd::sethostname(hostname)
        .with_context(|| format!("failed to set container hostname to {}", hostname))?;
    Ok(())
}

fn supervise_user_command(
    command: &[String],
    mut status_writer: Option<&mut std::fs::File>,
) -> Result<i32> {
    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::{fork, ForkResult};

    if command.is_empty() {
        anyhow::bail!("command cannot be empty");
    }

    let c_program = CString::new(command[0].as_str())
        .with_context(|| format!("invalid program path: {}", command[0]))?;
    let c_args: Result<Vec<CString>, _> = command
        .iter()
        .map(|arg| CString::new(arg.as_str()))
        .collect();
    let c_args = c_args.context("invalid argument string in command")?;

    match unsafe { fork()? } {
        ForkResult::Child => {
            let _ = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0));
            let _ = nix::unistd::execvp(&c_program, &c_args);
            eprintln!(
                "minidock: failed to execvp {}: {}",
                command[0],
                std::io::Error::last_os_error()
            );
            std::process::exit(127);
        }
        ForkResult::Parent { child } => {
            FORWARD_SIGNAL.store(0, Ordering::SeqCst);
            let sa = nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::Handler(signal_handler),
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            );
            unsafe {
                let _ = nix::sys::signal::sigaction(Signal::SIGTERM, &sa);
                let _ = nix::sys::signal::sigaction(Signal::SIGINT, &sa);
                let _ = nix::sys::signal::sigaction(Signal::SIGHUP, &sa);
            }

            let mut main_exit_code: Option<i32> = None;
            loop {
                let sig = FORWARD_SIGNAL.swap(0, Ordering::SeqCst);
                if sig != 0 {
                    if let Ok(sig_enum) = Signal::try_from(sig) {
                        let _ = nix::sys::signal::kill(Pid::from_raw(-child.as_raw()), sig_enum);
                    }
                }

                match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(status) => {
                        let exited_pid = match status {
                            WaitStatus::Exited(p, _) => Some(p),
                            WaitStatus::Signaled(p, _, _) => Some(p),
                            WaitStatus::Stopped(p, _) => Some(p),
                            _ => None,
                        };
                        let code = exit_code_from_wait_status(status);
                        if exited_pid == Some(child) {
                            main_exit_code = Some(code);
                        }
                        if main_exit_code.is_some() {
                            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                                Err(nix::errno::Errno::ECHILD) => break,
                                Ok(WaitStatus::StillAlive) => {
                                    let _ = nix::sys::signal::kill(
                                        Pid::from_raw(-child.as_raw()),
                                        Signal::SIGKILL,
                                    );
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(nix::errno::Errno::ECHILD) => break,
                    Err(e) => return Err(e).context("waitpid failed in supervisor"),
                }
            }

            let final_code = main_exit_code.unwrap_or(0);
            if let Some(ref mut w) = status_writer {
                let _ = write_status_message(w, &StatusMessage::Exit(final_code));
            }
            Ok(final_code)
        }
    }
}

pub fn init_container(args: InitArgs) -> Result<()> {
    require_root()?;
    let rootfs = args.rootfs.context("rootfs path is required")?;
    let hostname = args.hostname.unwrap_or_else(|| "minidock".to_string());
    if args.command.is_empty() {
        anyhow::bail!("command cannot be empty");
    }

    setup_isolated_filesystem(&rootfs)?;
    setup_hostname(&hostname)?;
    let code = supervise_user_command(&args.command, None)?;
    std::process::exit(code);
}

pub fn run(request: RunRequest, store: StateStore) -> Result<i32> {
    anyhow::ensure!(
        request.image.exists(),
        "image does not exist: {}",
        request.image.display()
    );
    anyhow::ensure!(!request.command.is_empty(), "command cannot be empty");
    anyhow::ensure!(
        request.hostname.len() <= 63,
        "hostname cannot exceed 63 bytes"
    );

    require_root()?;
    let cgroup_manager = CgroupManager::detect()?;

    let id = Uuid::new_v4();
    let rootfs_dir = store.create_container_dir(id)?;

    if let Err(e) = extract_rootfs(&request.image, &rootfs_dir) {
        let _ = std::fs::remove_dir_all(store.container_dir(id));
        return Err(e);
    }

    let limits = Limits {
        memory_bytes: request.memory_bytes,
        cpu_percent: request.cpu_percent,
    };
    let container_cgroups = match cgroup_manager.create(id, limits) {
        Ok(cg) => cg,
        Err(e) => {
            let _ = std::fs::remove_dir_all(store.container_dir(id));
            return Err(e);
        }
    };

    let cgroup_path = container_cgroups
        .memory
        .clone()
        .or_else(|| container_cgroups.cpu.clone())
        .or_else(|| {
            cgroup_manager
                .unified_mount
                .as_ref()
                .map(|m| m.join("minidock").join(id.to_string()))
        })
        .or_else(|| {
            cgroup_manager
                .memory_mount
                .as_ref()
                .map(|m| m.join("minidock").join(id.to_string()))
        })
        .unwrap_or_default();

    let (pid_rx, pid_tx) = nix::unistd::pipe().context("failed to create pid pipe")?;
    let (release_rx, release_tx) = nix::unistd::pipe().context("failed to create release pipe")?;
    let (status_rx, status_tx) = nix::unistd::pipe().context("failed to create status pipe")?;

    let log_path = store.log_path(id);
    let detached = request.detached;

    use nix::unistd::{fork, ForkResult};

    match unsafe { fork()? } {
        ForkResult::Parent {
            child: launcher_pid,
        } => {
            let _ = nix::unistd::close(pid_tx);
            let _ = nix::unistd::close(release_rx);
            let _ = nix::unistd::close(status_tx);

            let mut pid_file = unsafe { std::fs::File::from_raw_fd(pid_rx) };
            let mut release_file = unsafe { std::fs::File::from_raw_fd(release_tx) };
            let status_file = unsafe { std::fs::File::from_raw_fd(status_rx) };
            let mut status_reader = BufReader::new(status_file);

            let mut pid_bytes = [0u8; 4];
            if let Err(e) = pid_file.read_exact(&mut pid_bytes) {
                let _ = container_cgroups.remove_empty();
                let _ = std::fs::remove_dir_all(store.container_dir(id));
                return Err(e).context("failed to receive init PID from launcher");
            }
            drop(pid_file);
            let init_host_pid = i32::from_ne_bytes(pid_bytes);

            if let Err(e) = container_cgroups.attach(init_host_pid) {
                let _ = container_cgroups.remove_empty();
                let _ = std::fs::remove_dir_all(store.container_dir(id));
                return Err(e).context("failed to attach container to cgroups");
            }

            let state = ContainerState {
                version: 1,
                id,
                pid: init_host_pid,
                cgroup_path,
                rootfs: rootfs_dir.clone(),
                command: request.command.clone(),
                hostname: request.hostname.clone(),
                detached,
                started_at: time::OffsetDateTime::now_utc(),
                status: ContainerStatus::Running,
            };
            if let Err(e) = store.save(&state) {
                let _ = container_cgroups.remove_empty();
                let _ = std::fs::remove_dir_all(store.container_dir(id));
                return Err(e).context("failed to save initial container state");
            }

            if let Err(e) = release_file.write_all(&[1]) {
                let _ = store.remove(id);
                let _ = container_cgroups.remove_empty();
                return Err(e).context("failed to send release byte to init");
            }
            let _ = release_file.flush();
            drop(release_file);

            let first_msg = match read_status_message(&mut status_reader) {
                Ok(msg) => msg,
                Err(e) => {
                    let _ = std::fs::remove_file(store.state_dir().join(format!("{id}.json")));
                    let _ = container_cgroups.remove_empty();
                    return Err(e).context("failed to read startup status from container");
                }
            };

            match first_msg {
                StatusMessage::Ready => {
                    if detached {
                        println!("{}", id);
                        Ok(0)
                    } else {
                        match read_status_message(&mut status_reader) {
                            Ok(StatusMessage::Exit(code)) => {
                                let _ = container_cgroups.remove_empty();
                                Ok(code)
                            }
                            Ok(other) => {
                                anyhow::bail!(
                                    "unexpected message while waiting for container exit: {:?}",
                                    other
                                );
                            }
                            Err(_) => {
                                let status = nix::sys::wait::waitpid(launcher_pid, None)?;
                                let code = exit_code_from_wait_status(status);
                                let _ = container_cgroups.remove_empty();
                                Ok(code)
                            }
                        }
                    }
                }
                StatusMessage::Error(err) => {
                    let _ = std::fs::remove_file(store.state_dir().join(format!("{id}.json")));
                    let _ = container_cgroups.remove_empty();
                    anyhow::bail!("container startup error: {}", err);
                }
                other => {
                    let _ = std::fs::remove_file(store.state_dir().join(format!("{id}.json")));
                    let _ = container_cgroups.remove_empty();
                    anyhow::bail!("unexpected startup message: {:?}", other);
                }
            }
        }
        ForkResult::Child => {
            let _ = nix::unistd::close(pid_rx);
            let _ = nix::unistd::close(release_tx);
            let _ = nix::unistd::close(status_rx);

            if detached {
                let _ = nix::unistd::setsid();
                if let Ok(devnull) = std::fs::File::open("/dev/null") {
                    let _ = nix::unistd::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
                }
                if let Ok(log_file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let fd = log_file.as_raw_fd();
                    unsafe {
                        libc::dup2(fd, libc::STDOUT_FILENO);
                        libc::dup2(fd, libc::STDERR_FILENO);
                    }
                }
            }

            use nix::sched::{unshare, CloneFlags};
            let flags = CloneFlags::CLONE_NEWPID
                | CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWUTS
                | CloneFlags::CLONE_NEWIPC;

            let mut status_file = unsafe { std::fs::File::from_raw_fd(status_tx) };
            if let Err(e) = unshare(flags) {
                let _ = write_status_message(
                    &mut status_file,
                    &StatusMessage::Error(format!("unshare failed: {}", e)),
                );
                std::process::exit(1);
            }

            match unsafe { fork() } {
                Ok(ForkResult::Parent { child: init_pid }) => {
                    let mut pid_file = unsafe { std::fs::File::from_raw_fd(pid_tx) };
                    let pid_bytes = init_pid.as_raw().to_ne_bytes();
                    let _ = pid_file.write_all(&pid_bytes);
                    let _ = pid_file.flush();
                    drop(pid_file);
                    let _ = nix::unistd::close(release_rx);

                    match nix::sys::wait::waitpid(init_pid, None) {
                        Ok(status) => {
                            let exit_code = exit_code_from_wait_status(status);
                            let _ = write_status_message(
                                &mut status_file,
                                &StatusMessage::Exit(exit_code),
                            );
                            std::process::exit(exit_code);
                        }
                        Err(_) => {
                            std::process::exit(1);
                        }
                    }
                }
                Ok(ForkResult::Child) => {
                    let _ = nix::unistd::close(pid_tx);

                    let res = (|| -> Result<()> {
                        setup_isolated_filesystem(&rootfs_dir)?;
                        setup_hostname(&request.hostname)?;

                        let mut release_file = unsafe { std::fs::File::from_raw_fd(release_rx) };
                        let mut gate = [0u8; 1];
                        release_file
                            .read_exact(&mut gate)
                            .context("failed reading release byte")?;
                        drop(release_file);

                        write_status_message(&mut status_file, &StatusMessage::Ready)?;

                        let code =
                            supervise_user_command(&request.command, Some(&mut status_file))?;
                        std::process::exit(code);
                    })();

                    if let Err(err) = res {
                        let _ = write_status_message(
                            &mut status_file,
                            &StatusMessage::Error(format!("{:#}", err)),
                        );
                        std::process::exit(1);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    let _ = write_status_message(
                        &mut status_file,
                        &StatusMessage::Error(format!("fork init failed: {}", e)),
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;
    use nix::sys::wait::WaitStatus;
    use nix::unistd::Pid;

    #[test]
    fn ready_message_round_trips() {
        assert_eq!(
            StatusMessage::decode(&StatusMessage::Ready.encode()).unwrap(),
            StatusMessage::Ready
        );
    }

    #[test]
    fn signal_exit_is_reported_as_128_plus_signal() {
        assert_eq!(
            exit_code_from_wait_status(WaitStatus::Signaled(
                Pid::from_raw(7),
                Signal::SIGTERM,
                false
            )),
            143
        );
    }

    #[test]
    fn stop_uses_sigkill_after_the_timeout() {
        let mut signals = Vec::new();
        stop_with(
            &mut RecordingProcessOps::new(&mut signals),
            Pid::from_raw(7),
        )
        .unwrap();
        assert_eq!(signals, vec![Signal::SIGTERM, Signal::SIGKILL]);
    }

    #[test]
    fn exited_status_returns_code() {
        assert_eq!(
            exit_code_from_wait_status(WaitStatus::Exited(Pid::from_raw(7), 42)),
            42
        );
    }

    #[test]
    fn status_message_error_and_exit_round_trip() {
        let err_msg = StatusMessage::Error("failed to pivot".to_string());
        assert_eq!(StatusMessage::decode(&err_msg.encode()).unwrap(), err_msg);

        let exit_msg = StatusMessage::Exit(130);
        assert_eq!(StatusMessage::decode(&exit_msg.encode()).unwrap(), exit_msg);

        let pid_msg = StatusMessage::InitPid(4242);
        assert_eq!(StatusMessage::decode(&pid_msg.encode()).unwrap(), pid_msg);
    }

    #[test]
    fn stop_stops_early_if_process_not_alive() {
        struct EarlyDeadOps {
            signals: Vec<Signal>,
        }
        impl ProcessOps for EarlyDeadOps {
            fn kill(&mut self, _pid: Pid, signal: Signal) -> Result<()> {
                self.signals.push(signal);
                Ok(())
            }
            fn is_alive(&mut self, _pid: Pid) -> bool {
                false
            }
            fn sleep(&mut self, _duration: Duration) {}
        }
        let mut ops = EarlyDeadOps {
            signals: Vec::new(),
        };
        stop_with(&mut ops, Pid::from_raw(7)).unwrap();
        assert_eq!(ops.signals, vec![Signal::SIGTERM]);
    }
}
