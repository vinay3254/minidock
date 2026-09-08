use minidock::state::{is_pid_alive, ContainerState, ContainerStatus, StateStore};
use std::io::{Read, Write};
use std::path::PathBuf;
use time::OffsetDateTime;
use uuid::Uuid;

fn fixture_state(status: ContainerStatus, pid: i32) -> ContainerState {
    ContainerState {
        version: 1,
        id: Uuid::new_v4(),
        pid,
        cgroup_path: PathBuf::from("/sys/fs/cgroup/memory/minidock/test"),
        rootfs: PathBuf::from("/home/user/.minidock/containers/test/rootfs"),
        command: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
        ],
        hostname: "test-host".to_string(),
        detached: false,
        started_at: OffsetDateTime::now_utc(),
        status,
    }
}

#[test]
fn save_and_load_round_trip_a_running_record() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();
    let loaded = store.load(state.id).unwrap();
    assert_eq!(loaded, state);
}

#[test]
fn list_marks_a_nonexistent_pid_as_exited_without_rewriting_the_id() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, i32::MAX);
    store.save(&state).unwrap();
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, ContainerStatus::Exited);
    assert_eq!(listed[0].id, state.id);

    // Verify on-disk file was not rewritten and ID was preserved
    let raw_file =
        std::fs::File::open(store.state_dir().join(format!("{}.json", state.id))).unwrap();
    let raw_state: ContainerState = serde_json::from_reader(raw_file).unwrap();
    assert_eq!(raw_state.id, state.id);
    assert_eq!(raw_state.status, ContainerStatus::Running);
}

#[test]
fn atomic_save_overwrites_existing_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let mut state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();

    state.hostname = "updated-hostname".to_string();
    state.detached = true;
    state.status = ContainerStatus::Stopped;
    store.save(&state).unwrap();

    let loaded = store.load(state.id).unwrap();
    assert_eq!(loaded, state);
    assert_eq!(loaded.hostname, "updated-hostname");
    assert!(loaded.detached);
    assert_eq!(loaded.status, ContainerStatus::Stopped);

    let temp_file = store.state_dir().join(format!("{}.json.tmp", state.id));
    assert!(!temp_file.exists());
}

#[test]
fn mark_status_updates_saved_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();

    store
        .mark_status(state.id, ContainerStatus::Stopped)
        .unwrap();

    let reloaded = store.load(state.id).unwrap();
    assert_eq!(reloaded.status, ContainerStatus::Stopped);
}

#[test]
fn mark_status_on_nonexistent_container_fails() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    assert!(store
        .mark_status(Uuid::new_v4(), ContainerStatus::Stopped)
        .is_err());
}

#[test]
fn remove_cleans_up_state_and_container_dir() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);

    store.create_container_dir(state.id).unwrap();
    store.create_log(state.id).unwrap();
    store.save(&state).unwrap();

    let state_file = store.state_dir().join(format!("{}.json", state.id));
    assert!(state_file.exists());
    assert!(store.container_dir(state.id).exists());
    assert!(store.log_path(state.id).exists());

    store.remove(state.id).unwrap();

    assert!(!state_file.exists());
    assert!(!store.container_dir(state.id).exists());
    assert!(store.load(state.id).is_err());

    // Idempotent removal of nonexistent container succeeds
    store.remove(state.id).unwrap();
}

#[test]
fn load_on_nonexistent_container_fails() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    assert!(store.load(Uuid::new_v4()).is_err());
}

#[test]
fn atomic_save_leaves_no_temp_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();

    let state_dir = store.state_dir();
    let target = state_dir.join(format!("{}.json", state.id));
    let temp_file = state_dir.join(format!("{}.json.tmp", state.id));

    assert!(target.exists());
    assert!(!temp_file.exists());
}

#[test]
fn directory_helpers_return_consistent_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let store = StateStore::at(root.clone());
    let id = Uuid::new_v4();

    assert_eq!(store.root(), root.as_path());
    assert_eq!(store.state_dir(), root.join("state"));
    assert_eq!(store.containers_dir(), root.join("containers"));
    assert_eq!(
        store.container_dir(id),
        root.join("containers").join(id.to_string())
    );
    assert_eq!(
        store.rootfs_dir(id),
        root.join("containers").join(id.to_string()).join("rootfs")
    );
    assert_eq!(store.rootfs_path(id), store.rootfs_dir(id));
    assert_eq!(
        store.log_path(id),
        root.join("containers")
            .join(id.to_string())
            .join("container.log")
    );

    let created = store.create_container_dir(id).unwrap();
    assert_eq!(created, store.rootfs_dir(id));
    assert!(created.is_dir());
    assert!(store.container_dir(id).is_dir());
}

#[test]
fn log_create_and_open_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let id = Uuid::new_v4();

    // Opening nonexistent log should fail
    assert!(store.open_log(id).is_err());

    // Creating log file and writing to it
    let mut file = store.create_log(id).unwrap();
    file.write_all(b"container log entry 1\n").unwrap();
    drop(file);

    // Opening log file and reading from it
    let mut read_file = store.open_log(id).unwrap();
    let mut content = String::new();
    read_file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "container log entry 1\n");
}

#[test]
fn list_sorts_by_started_at_ascending() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());

    let base_time = OffsetDateTime::now_utc();
    let mut s1 = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    s1.started_at = base_time - time::Duration::seconds(100);

    let mut s2 = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    s2.started_at = base_time - time::Duration::seconds(50);

    let mut s3 = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    s3.started_at = base_time;

    // Save out of order
    store.save(&s2).unwrap();
    store.save(&s3).unwrap();
    store.save(&s1).unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].id, s1.id);
    assert_eq!(listed[1].id, s2.id);
    assert_eq!(listed[2].id, s3.id);
}

#[test]
fn list_on_empty_or_nonexistent_directory_returns_empty_vec() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().join("nonexistent"));
    assert_eq!(store.list().unwrap(), Vec::new());
}

#[test]
fn list_ignores_non_json_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();

    // Write a dummy file in state dir
    std::fs::write(store.state_dir().join("not-json.txt"), b"ignore me").unwrap();
    std::fs::write(store.state_dir().join("some.json.tmp"), b"ignore me too").unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, state.id);
}

#[test]
fn list_validates_uuid_in_json_filenames() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    std::fs::create_dir_all(store.state_dir()).unwrap();

    // Write a non-uuid JSON file
    std::fs::write(store.state_dir().join("not-a-uuid.json"), b"{}").unwrap();

    let result = store.list();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("invalid container UUID in filename"));
}

#[test]
fn list_validates_id_matches_state_record() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Running, std::process::id() as i32);
    store.save(&state).unwrap();

    // Create another file whose name is a different UUID but contains state.id
    let different_id = Uuid::new_v4();
    let file =
        std::fs::File::create(store.state_dir().join(format!("{different_id}.json"))).unwrap();
    serde_json::to_writer(file, &state).unwrap();

    let result = store.list();
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("container ID mismatch"));
}

#[test]
fn stopped_container_is_not_reconciled_to_exited() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::at(temp.path().to_path_buf());
    let state = fixture_state(ContainerStatus::Stopped, i32::MAX);
    store.save(&state).unwrap();

    let loaded = store.load(state.id).unwrap();
    assert_eq!(loaded.status, ContainerStatus::Stopped);

    let listed = store.list().unwrap();
    assert_eq!(listed[0].status, ContainerStatus::Stopped);
}

#[test]
fn serde_matches_design_spec_json() {
    let json_text = r#"{
      "version": 1,
      "id": "a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d",
      "pid": 12345,
      "cgroup_path": "/sys/fs/cgroup/memory/minidock/a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d",
      "rootfs": "/home/user/.minidock/containers/a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d/rootfs",
      "command": ["/bin/sh"],
      "hostname": "a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d",
      "detached": true,
      "started_at": "2026-09-06T12:34:56Z",
      "status": "running"
    }"#;

    let state: ContainerState = serde_json::from_str(json_text).unwrap();
    assert_eq!(state.version, 1);
    assert_eq!(state.status, ContainerStatus::Running);
    assert_eq!(
        state.id,
        Uuid::parse_str("a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d").unwrap()
    );

    // Serialize and verify status is lowercase
    let serialized = serde_json::to_string(&state).unwrap();
    assert!(serialized.contains("\"status\":\"running\""));
}

#[test]
fn is_pid_alive_identifies_running_and_dead_processes() {
    // Current process is alive
    assert!(is_pid_alive(std::process::id() as i32));

    // Nonexistent PID
    assert!(!is_pid_alive(i32::MAX));

    // Zero or negative PIDs are treated as dead / invalid
    assert!(!is_pid_alive(0));
    assert!(!is_pid_alive(-1));
    assert!(!is_pid_alive(-42));
}

#[test]
fn from_current_user_points_to_home_minidock() {
    let home = std::env::var_os("HOME").unwrap();
    let store = StateStore::from_current_user().unwrap();
    assert_eq!(
        store.root(),
        PathBuf::from(home).join(".minidock").as_path()
    );
}

#[test]
fn container_status_display_formats_correctly() {
    assert_eq!(ContainerStatus::Running.to_string(), "running");
    assert_eq!(ContainerStatus::Exited.to_string(), "exited");
    assert_eq!(ContainerStatus::Stopped.to_string(), "stopped");
}
