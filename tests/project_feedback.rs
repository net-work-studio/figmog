#![recursion_limit = "256"]

//! Regression coverage for project discovery and migration review feedback.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const KEY: &str = "SyntheticProjectFeedback0001";

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn figmog(cwd: &Path) -> assert_cmd::Command {
    let mut command = common::command();
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn fixture(dir: &Path) -> PathBuf {
    let path = dir.join("response.json");
    fs::write(&path, serde_json::to_vec(&common::fixture_v1()).unwrap()).unwrap();
    path
}

#[test]
fn missing_git_does_not_create_a_second_mirror_in_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    let input = fixture(dir.path());
    let out = figmog(dir.path())
        .env("PATH", "/nonexistent")
        .args(["--no-socket", "pull", "--from-file"])
        .arg(input)
        .arg(KEY)
        .assert()
        .failure();
    let error = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(
        error.contains("refusing a local mirror fallback"),
        "{error}"
    );
    assert!(!dir.path().join(".figmog").join(KEY).exists());
    assert!(!dir.path().join(".git/figmog").join(KEY).exists());
}

#[test]
fn an_old_subdirectory_mirror_remains_readable_and_can_be_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("packages/app");
    fs::create_dir_all(&sub).unwrap();
    // Before this was a Git repository, the old cwd-relative layout was
    // also the current non-Git layout. This creates a real synthetic store.
    figmog(&sub)
        .args(["--no-socket", "pull", "--from-file"])
        .arg(fixture(dir.path()))
        .arg(KEY)
        .assert()
        .success();
    git(dir.path(), &["init", "--quiet"]);
    figmog(&sub)
        .args(["--no-socket", "status"])
        .assert()
        .success();
    let out = figmog(&sub).arg("migrate").assert().success();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(report["failed"].as_array().unwrap().is_empty());
    assert_eq!(report["migrated"][0]["file"], KEY);
    assert!(!sub.join(".figmog").join(KEY).exists());
    assert!(dir.path().join(".git/figmog").join(KEY).join("db").exists());
    figmog(&sub)
        .args(["--no-socket", "status"])
        .assert()
        .success();
}

#[test]
fn migrate_prints_successes_and_failures_and_returns_failure() {
    let dir = tempfile::tempdir().unwrap();
    let input = fixture(dir.path());
    let failed = "AaaaSyntheticProject";
    let successful = "ZzzzSyntheticProject";
    for key in [failed, successful] {
        figmog(dir.path())
            .args(["--no-socket", "pull", "--from-file"])
            .arg(&input)
            .arg(key)
            .assert()
            .success();
    }
    git(dir.path(), &["init", "--quiet"]);
    fs::create_dir_all(dir.path().join(".git/figmog").join(failed).join("db")).unwrap();
    let out = figmog(dir.path()).arg("migrate").assert().failure();
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["failed"][0]["file"], failed);
    assert_eq!(report["migrated"][0]["file"], successful);
    assert!(dir.path().join(".figmog").join(failed).join("db").exists());
    assert!(
        dir.path()
            .join(".git/figmog")
            .join(successful)
            .join("db")
            .exists()
    );
}
