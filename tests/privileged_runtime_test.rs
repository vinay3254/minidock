use assert_cmd::Command;

fn require_root_and_v1() {
    if unsafe { libc::geteuid() } != 0 {
        panic!("test requires root privileges");
    }
    if minidock::CgroupManager::detect().is_err() {
        panic!("test requires cgroups v1 controllers");
    }
}

#[test]
#[ignore = "requires root, cgroups v1, and MINIDOCK_TEST_IMAGE"]
fn detached_container_is_namespaced_limited_logged_and_stoppable() {
    require_root_and_v1();
    let image = std::env::var_os("MINIDOCK_TEST_IMAGE").expect("set MINIDOCK_TEST_IMAGE");
    let output = Command::cargo_bin("minidock")
        .unwrap()
        .args([
            "run",
            "-d",
            "--image",
            image.to_str().unwrap(),
            "--memory",
            "32M",
            "--cpu-percent",
            "50",
            "--hostname",
            "mini-test",
            "--",
            "/bin/sh",
            "-c",
            "hostname; cat /proc/1/comm; echo ready; sleep 60",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "run failed: {:?}", output);
    let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert!(!id.is_empty());

    let ps_output = Command::cargo_bin("minidock")
        .unwrap()
        .arg("ps")
        .output()
        .unwrap();
    assert!(ps_output.status.success());
    let ps_stdout = String::from_utf8(ps_output.stdout).unwrap();
    let short_id = if id.len() >= 12 { &id[..12] } else { &id };
    assert!(
        ps_stdout.contains(short_id),
        "ps output missing container ID: {}",
        ps_stdout
    );

    let mut log_text = String::new();
    for _ in 0..50 {
        let logs_output = Command::cargo_bin("minidock")
            .unwrap()
            .args(["logs", &id])
            .output()
            .unwrap();
        if logs_output.status.success() {
            log_text = String::from_utf8_lossy(&logs_output.stdout).to_string();
            if log_text.contains("ready") {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        log_text.contains("mini-test"),
        "logs missing hostname: {}",
        log_text
    );
    assert!(
        log_text.contains("ready"),
        "logs missing ready marker: {}",
        log_text
    );

    let stop_output = Command::cargo_bin("minidock")
        .unwrap()
        .args(["stop", &id])
        .output()
        .unwrap();
    assert!(
        stop_output.status.success(),
        "stop failed: {:?}",
        stop_output
    );
}

#[test]
#[ignore = "fixture-driven operational suite test"]
fn hostile_archive_traversal_is_prevented() {
    let temp_dir = tempfile::tempdir().unwrap();
    let evil_tar_path = temp_dir.path().join("evil.tar.gz");

    {
        let file = std::fs::File::create(&evil_tar_path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);

        let data = b"pwned";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        let name_bytes = b"../../outside.txt";
        header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_cksum();

        builder.append(&header, &data[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    let extract_dir = temp_dir.path().join("extract");
    let outside_path = temp_dir.path().join("outside.txt");

    let result = minidock::extract_rootfs(&evil_tar_path, &extract_dir);
    assert!(
        result.is_err(),
        "extraction should reject ../../outside.txt"
    );
    assert!(!outside_path.exists(), "outside path must not be created");
}
