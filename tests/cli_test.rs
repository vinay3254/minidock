use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn run_requires_a_command_after_the_separator() {
    Command::cargo_bin("minidock")
        .unwrap()
        .args(["run", "--image", "rootfs.tar.gz"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("command"));
}

#[test]
fn cpu_percent_must_be_between_one_and_one_hundred() {
    Command::cargo_bin("minidock")
        .unwrap()
        .args([
            "run",
            "--image",
            "rootfs.tar.gz",
            "--cpu-percent",
            "101",
            "--",
            "/bin/sh",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1..=100"));
}

#[test]
fn logs_rejects_an_invalid_container_id_before_accessing_state() {
    Command::cargo_bin("minidock")
        .unwrap()
        .args(["logs", "not-a-uuid"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid container ID"));
}

#[test]
fn run_reports_a_missing_image_before_root_preflight() {
    Command::cargo_bin("minidock")
        .unwrap()
        .args(["run", "--image", "/does/not/exist", "--", "/bin/sh"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("image does not exist"));
}
