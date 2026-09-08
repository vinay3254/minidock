# minidock v1 Design

## Purpose

minidock is a deliberately small, root-required Linux container runtime written in
Rust. It runs a command from a local `.tar.gz` root filesystem with isolated PID,
mount, UTS, and IPC namespaces, and applies cgroups-v1 CPU and memory limits. It
does not implement networking, OCI images, layered images, cgroups v2, or rootless
operation.

The v1 success case is:

```bash
minidock run --image busybox-rootfs.tar.gz --memory 128M --cpu-percent 50 -- /bin/sh
```

The shell is PID 1 in its own PID namespace, sees only its own process tree through
a freshly mounted `/proc`, has the requested hostname and cgroup limits, and can be
stopped and inspected from the host.

## Scope and Constraints

- Linux only; the invoking user must be root or have the capabilities needed for
  namespace, mount, and cgroup operations.
- Rust 1.70 or newer.
- Images are trusted, local gzip-compressed tar archives. This is not an OCI image
  format or registry client.
- cgroups v1 is required for v1. A unified cgroups-v2-only host fails before a
  container is started with an explicit unsupported-host error.
- Every container has a UUID identifier and state below `~/.minidock` (resolved via
  the current user's home directory, including when run with sudo).
- No user-controlled input may escape the minidock state directory, image extraction
  destination, or cgroup subtree.

## User-facing CLI

`main.rs` uses clap subcommands and dispatches only; implementation details stay in
the module owning them.

```text
minidock run [--image PATH] [--memory SIZE] [--cpu-percent 1..=100]
             [--hostname NAME] [-d|--detach] -- COMMAND [ARG ...]
minidock ps
minidock stop CONTAINER_ID
minidock logs CONTAINER_ID
minidock build --context DIRECTORY --output IMAGE.tar.gz
```

`run` requires an image and a command. Defaults are: no memory limit, no CPU limit,
hostname equal to the short container ID, and foreground execution. Detached mode
redirects command stdout and stderr to `<container-dir>/container.log`; foreground
mode inherits the caller's streams. `logs` prints the saved log and returns a clear
error if the container was started in foreground mode, where no persistent log exists.

`ps` lists saved containers, their short IDs, recorded command, host PID, status,
and start timestamp. Before displaying a record it checks whether the host PID is
alive; dead records display as `exited`. `stop` sends SIGTERM, waits up to 10 seconds,
then sends SIGKILL if needed; it updates the record to `stopped` but preserves it and
the log for inspection. Automatic rootfs/cgroup garbage collection is explicitly out
of scope for v1.

`build` packs a directory into a gzip tar archive. It rejects non-directories and
stores entries relative to the context directory; it does not interpret Dockerfiles.

## Files and Module Boundaries

```text
minidock/
├── Cargo.toml
├── src/
│   ├── lib.rs           public module declarations and shared typed interfaces
│   ├── main.rs          clap schema, argument validation, command dispatch
│   ├── container.rs     process lifecycle, namespace setup, mount/pivot/init logic
│   ├── cgroups.rs       v1 mount discovery, cgroup creation, limit writes, cleanup
│   ├── image.rs         safe tar extraction and deterministic archive creation
│   └── state.rs         state paths, JSON records, logs, PID/status handling
├── tests/
│   ├── image_test.rs    archive round-trips and path-traversal rejection
│   ├── state_test.rs    state persistence and stale-PID classification
│   └── cli_test.rs      clap validation and error reporting
└── docs/superpowers/
    ├── specs/
    └── plans/
```

`lib.rs` exposes the modules to integration tests while keeping the binary entrypoint
thin. `state.rs` owns the on-disk layout and exposes a `ContainerState` record, avoiding
filesystem knowledge in other modules. `image.rs` owns all archive handling and
returns only a validated rootfs directory. `cgroups.rs` never spawns processes: it
receives a generated ID and a host PID. `container.rs` coordinates those modules and
contains the only unsafe syscall boundaries. `main.rs` converts clap values into
typed inputs and renders output/errors.

## Persistent State

For ID `abc123`, paths are:

```text
~/.minidock/
├── containers/abc123/
│   ├── rootfs/
│   └── container.log       # detached containers only
└── state/abc123.json
```

The JSON schema is versioned from the start:

```json
{
  "version": 1,
  "id": "abc123...",
  "pid": 12345,
  "cgroup_path": "/sys/fs/cgroup/memory/minidock/abc123...",
  "rootfs": "/home/user/.minidock/containers/abc123.../rootfs",
  "command": ["/bin/sh"],
  "hostname": "abc123",
  "detached": true,
  "started_at": "2026-09-06T12:34:56Z",
  "status": "running"
}
```

State writes are atomic: serialize to a same-directory temporary file, `sync_all`,
then rename it to `<id>.json`. IDs must be UUIDs parsed by the program before being
used in any path. State is written only after the parent has successfully created
the child and attached its host PID to the cgroup. If a later setup stage reports
failure through the startup pipe, the parent removes the incomplete state record
and reports the child error.

## Image Handling

`extract_rootfs(image, destination)` creates the destination itself and rejects a
tar entry whose resolved path leaves it, an absolute archive path, device nodes,
FIFOs, and hard links or symlinks that resolve outside the destination. It preserves
ordinary files, directories, permissions, and in-root relative symlinks. Extraction
failure removes only the just-created container directory.

`build_image(context, output)` walks the context without following symlinks and
writes a gzip tar with relative paths. It may include in-root symlinks but rejects
special files. The command fails if `output` is within `context`, preventing the
archive from including itself.

## Cgroups v1

At startup, `CgroupManager::discover()` parses `/proc/self/mountinfo` and
`/proc/self/cgroup` to find separate v1 controllers for `memory` and `cpu`. It
returns `UnsupportedCgroupVersion` if a v2 unified hierarchy is the available
configuration. It creates `minidock/<uuid>` beneath each controller mount.

- Memory: parse binary-suffix input (`B`, `K`, `M`, `G`, case-insensitive) into bytes
  and write the decimal value to `memory.limit_in_bytes`.
- CPU: read `cpu.cfs_period_us` (falling back to `100000` if it does not exist),
  calculate `max(1000, period * percent / 100)`, and write it to `cpu.cfs_quota_us`.
- Attachment: write the child host PID to each cgroup's `tasks` file.
- Cleanup: remove an empty per-container cgroup only after the process has exited;
  cleanup failures are reported but never hide the primary run/stop result.

The parent attaches the child by host PID. The init process must not rely on a
host-mounted cgroup filesystem after `pivot_root`.

## Process and Namespace Lifecycle

The implementation uses an explicit parent/launcher/init protocol with pipes for
host-PID handoff, cgroup-release, and ready/error/exit status. This makes the recorded
PID and cgroup attachment deterministic, preserves a parent able to reap namespace
init, and prevents a state record for an init process that failed before exec.

1. The host parent creates a UUID, extracts the rootfs, discovers/creates cgroups,
   opens a startup pipe, and forks a launcher child.
2. The launcher calls `unshare(CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUTS |
   CLONE_NEWIPC)`. `CLONE_NEWPID` only applies to future children, so it then forks
   again.
3. The launcher sends the init child's host PID to the original parent but remains its
   parent and waits for it. The original parent uses that host PID to attach the init
   process to both cgroups, writes its JSON state, then sends an explicit release byte
   through the cgroup-release pipe. Until it receives that byte, init cannot run the
   requested command.
4. The second child is PID 1 in the new PID namespace. It re-execs the same binary as
   hidden `init-container`, passing only validated rootfs, hostname, and command
   arguments. Re-exec gives PID 1 a clean process image and keeps init-only setup out
   of public CLI dispatch.
5. `init-container` makes `/` recursively private, bind-mounts the rootfs to itself,
   creates `<rootfs>/.old_root`, performs `pivot_root(rootfs, .old_root)`, changes to
   `/`, lazily unmounts `/.old_root`, and removes that directory.
6. It mounts a fresh `proc` filesystem at `/proc`, calls `sethostname`, waits for the
   cgroup-release byte, and sends ready to the parent. Failures before launching the
   user command are written to the status pipe and cause a nonzero exit.
7. The init supervisor forks the requested command into a new process group, forwards
   SIGTERM/SIGINT/SIGHUP to that group, waits and reaps all children, then sends the
   main command's exit status to the launcher and exits with it. The launcher forwards
   ready/error/exit messages to the original parent. In foreground mode, that parent
   waits for the relayed exit status and returns it. In detached mode, it returns after
   readiness while the launcher continues to reap init.

PID 1 must reap children. The init process intentionally does not directly `execvp`
the user command: it remains a small supervisor so it can reap descendants and relay
termination signals. This avoids zombie processes and makes `stop` work predictably.

## Failure Handling and Safety

- Validate image existence, command, memory syntax, CPU range, hostname length/bytes,
  root privilege/capabilities, and cgroup support before forking.
- Every syscall error includes the operation and relevant path, preserving `errno`
  context through `anyhow`.
- Mount propagation is made private before any mount or pivot operation.
- The host parent never calls `pivot_root`, changes its hostname, or mounts `/proc`.
- Startup errors are propagated across pipes, and the host cleans only resources for
  the newly generated ID.
- `stop` treats `ESRCH` as already exited and never signals a PID without first loading
  and validating its ID's state file.
- `logs` and `ps` never execute commands or parse shell strings from state; commands
  remain JSON string arrays.

## Test Strategy

Unit tests run unprivileged and cover pure parsing, archive safety, state serialization,
atomic replacement behavior, cgroup quota calculation, and clap validation. Syscall
operations are placed behind small interfaces so they can be recorded/faked in unit
tests, while production uses nix/libc implementations.

Privileged integration tests are marked `#[ignore]` and require Linux, root,
cgroups v1, and a checked-in or generated BusyBox rootfs fixture. They verify:

1. `/proc/1` inside the container represents the container init rather than host PID
   1, and hostname isolation does not alter the host hostname.
2. A memory limit creates and populates the correct cgroup-v1 control file.
3. A detached `sleep` appears in `ps`, receives `stop`, and produces a saved log.
4. A malicious tar entry such as `../../outside` cannot create a host file.

The default test suite must never require root or mutate `/sys/fs/cgroup`.

## Alternatives Considered

1. **One `fork` followed by `unshare` and direct exec** — rejected because the caller
   does not become PID 1 when `CLONE_NEWPID` is unshared; only children created after
   the call join the new PID namespace.
2. **Use `clone` with namespace flags** — viable and shorter, but rejected for v1 in
   favor of the explicit launcher/init protocol because it makes host-PID handoff,
   startup error propagation, reaping, and the PID-namespace rule easier to review
   and test.
3. **Make the user command PID 1 via direct exec** — rejected because PID 1 must reap
   descendants and relay termination signals. A tiny Rust init supervisor is required.
4. **Implement cgroups v2 now** — deferred to the stated roadmap. Detecting and
   refusing a v2-only host avoids silently running without resource limits.

## Acceptance Criteria

- `cargo build --release` succeeds on supported Linux/Rust versions.
- The four public runtime modules exist with the boundaries in this document.
- `run` executes a command after a safe rootfs extraction, in PID/mount/UTS/IPC
  namespaces, with a fresh `/proc` and isolated hostname.
- Requested cgroups-v1 memory and CPU limits are written and the init host PID is
  attached before the command is released to run.
- `ps`, `stop`, `logs`, and `build` meet the behavior specified above.
- Unprivileged unit tests pass; privileged integration tests are opt-in and document
  their host prerequisites.
- Unsupported privilege, mount, cgroup, archive, and startup conditions fail clearly
  without modifying unrelated host state.
