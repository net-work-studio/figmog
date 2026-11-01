#![recursion_limit = "256"]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const KEY: &str = "figmogignorefixture123";

fn git() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn figmog() -> assert_cmd::Command {
    let mut command = common::command();
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        git()
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    dir
}

fn response(cwd: &Path) -> PathBuf {
    let path = cwd.join("response.json");
    fs::write(&path, serde_json::to_vec(&common::fixture_v1()).unwrap()).unwrap();
    path
}

fn pull(cwd: &Path) {
    figmog()
        .current_dir(cwd)
        .args(["pull", KEY, "--from-file"])
        .arg(response(cwd))
        .assert()
        .success();
}

#[test]
fn successful_pull_updates_the_nearest_ignore_once_and_preserves_existing_bytes() {
    let repo = repository();
    let nested = repo.path().join("packages/app");
    fs::create_dir_all(&nested).unwrap();
    fs::write(repo.path().join(".gitignore"), "root-entry\n").unwrap();
    let ignore = repo.path().join("packages/.gitignore");
    fs::write(&ignore, "# existing\r\nnode_modules/").unwrap();
    pull(&nested);
    pull(&nested);
    assert_eq!(
        fs::read(&ignore).unwrap(),
        b"# existing\r\nnode_modules/\r\n.figmog/\r\n"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join(".gitignore")).unwrap(),
        "root-entry\n"
    );
    assert!(!nested.join(".gitignore").exists());
    assert!(
        git()
            .args(["check-ignore", "--quiet", ".figmog/"])
            .current_dir(&nested)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn existing_broad_ignore_is_respected_and_nested_negations_are_overridden() {
    let repo = repository();
    let ignore = repo.path().join(".gitignore");
    fs::write(&ignore, ".fig*/\n").unwrap();
    pull(repo.path());
    assert_eq!(fs::read_to_string(&ignore).unwrap(), ".fig*/\n");
    let nested = repo.path().join("app");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join(".gitignore"), "!.figmog/\n").unwrap();
    pull(&nested);
    assert_eq!(
        fs::read_to_string(nested.join(".gitignore")).unwrap(),
        "!.figmog/\n.figmog/\n"
    );
}

#[test]
fn missing_ignore_is_created_at_repository_root_but_never_outside_a_repository() {
    let repo = repository();
    let nested = repo.path().join("app");
    fs::create_dir(&nested).unwrap();
    pull(&nested);
    assert_eq!(
        fs::read_to_string(repo.path().join(".gitignore")).unwrap(),
        ".figmog/\n"
    );
    assert!(!nested.join(".gitignore").exists());

    let plain = tempfile::tempdir().unwrap();
    fs::write(plain.path().join(".gitignore"), "existing\n").unwrap();
    pull(plain.path());
    assert_eq!(
        fs::read_to_string(plain.path().join(".gitignore")).unwrap(),
        "existing\n"
    );
}

#[test]
fn explicit_db_and_failed_pulls_do_not_modify_ignore_rules() {
    let repo = repository();
    figmog()
        .current_dir(repo.path())
        .args(["pull", KEY, "--from-file"])
        .arg(response(repo.path()))
        .arg("--db")
        .arg(repo.path().join("custom-db"))
        .assert()
        .success();
    assert!(!repo.path().join(".gitignore").exists());
    figmog()
        .current_dir(repo.path())
        .args(["pull", KEY, "--from-file", "missing-response.json"])
        .assert()
        .failure();
    assert!(!repo.path().join(".gitignore").exists());
    assert!(!repo.path().join(".figmog").exists());
}

#[test]
fn an_unwritable_ignore_does_not_fail_an_already_successful_pull() {
    let repo = repository();
    fs::create_dir(repo.path().join(".gitignore")).unwrap();
    let out = figmog()
        .current_dir(repo.path())
        .args(["pull", KEY, "--from-file"])
        .arg(response(repo.path()))
        .assert()
        .success();
    let output = out.get_output();
    let churn: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(churn["added"], 18);
    let warning: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        warning["warning"]
            .as_str()
            .unwrap()
            .contains("could not ignore .figmog/")
    );
}

#[test]
fn worktrees_with_a_git_file_are_recognized() {
    let repo = repository();
    let git_dir = repo.path().join(".git");
    let relocated = repo.path().join("git-metadata");
    fs::rename(&git_dir, &relocated).unwrap();
    fs::write(&git_dir, "gitdir: git-metadata\n").unwrap();
    pull(repo.path());
    assert_eq!(
        fs::read_to_string(repo.path().join(".gitignore")).unwrap(),
        ".figmog/\n"
    );
}

#[test]
fn symlinked_ignore_is_not_followed_outside_the_repository() {
    let repo = repository();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("ignore");
    fs::write(&target, "existing\n").unwrap();
    std::os::unix::fs::symlink(&target, repo.path().join(".gitignore")).unwrap();
    let out = figmog()
        .current_dir(repo.path())
        .args(["pull", KEY, "--from-file"])
        .arg(response(repo.path()))
        .assert()
        .success();
    assert_eq!(fs::read_to_string(target).unwrap(), "existing\n");
    let warning: serde_json::Value = serde_json::from_slice(&out.get_output().stderr).unwrap();
    assert!(
        warning["warning"]
            .as_str()
            .unwrap()
            .contains("not a regular file")
    );
}
