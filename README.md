# minidock

A lightweight, educational Linux container runtime written in Rust.

`minidock` implements container isolation using Linux namespaces (PID, Mount, UTS, IPC), cgroups v1 resource controls (CPU and memory limits), and a safe `.tar.gz` rootfs image format.

---

## Features

- **Linux Namespace Isolation**: Isolates PID, Mount, UTS (hostname), and IPC namespaces.
- **cgroups v1 Resource Limits**:
  - Memory limits (`--memory 128M`, `1G`, etc.) via `memory.limit_in_bytes`.
  - CPU quotas (`--cpu-percent 50`) via CFS quota and period controls (`cpu.cfs_quota_us`).
- **Safe Rootfs Image Format**:
  - Packages directories into gzip-compressed tarballs (`minidock build`).
  - Strict extraction security: rejects path traversal, directory-escaping symlinks, hard links, and FIFO entries.
- **Container Lifecycle & State Management**:
  - Atomic state persistence (`~/.minidock/containers/<id>/state.json`).
  - Foreground and detached (`-d`) execution modes.
  - Active process health checking and status tracking (`running`, `stopped`, `exited`).
  - Graceful shutdown (`minidock stop`) with SIGTERM and SIGKILL escalation.
  - Detached container log retrieval (`minidock logs`).

---

## Architecture

```text
minidock/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Core library exports and data types
│   ├── main.rs          # CLI schema (clap) and command dispatch
│   ├── container.rs     # Process lifecycle, namespaces, mount/pivot_root setup
│   ├── cgroups.rs       # cgroups v1 controller discovery and limit enforcement
│   ├── image.rs         # Safe tarball extraction and image builder
│   └── state.rs         # Atomic JSON state records and container logs
├── tests/
│   ├── cli_test.rs      # CLI validation tests
│   ├── image_test.rs    # Safe rootfs archive round-trip and security tests
│   ├── state_test.rs    # State persistence, serialization, and PID reconciliation tests
│   └── privileged_runtime_test.rs # Opt-in privileged runtime smoke tests
└── docs/
    └── superpowers/     # Architecture specs and development plans
```

---

## Requirements & Scope Constraints

- **OS**: Linux (kernel with PID, Mount, UTS, IPC namespaces and cgroups v1 support).
- **Permissions**: Root (`sudo`) or `CAP_SYS_ADMIN` capabilities for namespace, mount, and cgroup operations.
- **cgroups v1**: minidock v1 strictly requires cgroups v1 (hosts providing only cgroups v2 are rejected).
- **Image Format**: Local trusted gzip-compressed tar archives only.
- **Out of Scope for v1**: Networking, OCI registry image pulling, layered filesystems (overlayfs), rootless containers, and automatic rootfs cleanup.
- **Toolchain**: Rust 1.70+.

---

## Installation & Build

Build with Cargo:

```bash
cargo build --release
```

Run the unprivileged test suite:

```bash
cargo test
```

---

## Usage Guide

### 1. Host Prerequisite: Preparing a Rootfs Image

To prepare a BusyBox rootfs archive for testing (host prerequisite):

```bash
docker create --name bb-temp busybox
docker export bb-temp | gzip > busybox-rootfs.tar.gz
docker rm bb-temp
```

Or build an archive from an existing rootfs directory with `minidock build`:

```bash
minidock build --context ./rootfs-dir --output ./busybox-rootfs.tar.gz
```

### 2. Run a Container

Run a command interactively with memory and CPU limits:

```bash
sudo ./target/release/minidock run --image ./busybox-rootfs.tar.gz --memory 128M --cpu-percent 50 -- /bin/sh
```

Run in detached mode in the background:

```bash
sudo ./target/release/minidock run -d --image ./busybox-rootfs.tar.gz --hostname web-box -- /bin/sh -c "while true; do echo 'tick'; sleep 1; done"
```

### 3. List Containers

```bash
./target/release/minidock ps
```

### 4. Inspect Container Logs

```bash
sudo ./target/release/minidock logs <uuid>
```

### 5. Stop a Container

```bash
sudo ./target/release/minidock stop <uuid>
```

---

## Privileged Integration Testing

Privileged integration tests that verify namespace isolation, cgroup limits, and background logging are opt-in and require root privileges and cgroups v1:

```bash
MINIDOCK_TEST_IMAGE=$PWD/busybox-rootfs.tar.gz sudo -E cargo test --test privileged_runtime_test -- --ignored --test-threads=1
```

---

## License

MIT
