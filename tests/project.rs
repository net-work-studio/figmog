#![recursion_limit = "256"]

//! Project config and mirror-root discovery end to end (spec §16).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const KEY: &str = "figmogprojectfixture123";

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=figmog",
            "-c",
            "user.email=figmog@example.invalid",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "git {args:?}");
}

fn figmog(cwd: &Path) -> assert_cmd::Command {
    let mut command = common::command();
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn response(dir: &Path) -> PathBuf {
    let path = dir.join("response.json");
    fs::write(&path, serde_json::to_vec(&common::fixture_v1()).unwrap()).unwrap();
    path
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

/// `<tmp>/main` (a repo with one commit) inside a canonical temp dir.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let main = fs::canonicalize(dir.path()).unwrap().join("main");
    fs::create_dir(&main).unwrap();
    git(&main, &["init", "--quiet"]);
    (dir, main)
}

#[test]
fn worktrees_and_subdirectories_share_one_mirror_named_by_figmog_toml() {
    let (dir, main) = repository();
    fs::write(
        main.join("figmog.toml"),
        format!("# synthetic project\n[files]\ndefault = \"{KEY}\"\n"),
    )
    .unwrap();
    git(&main, &["add", "figmog.toml"]);
    git(&main, &["commit", "--quiet", "-m", "init"]);
    let linked = dir.path().join("linked");
    git(
        &main,
        &["worktree", "add", "--quiet", linked.to_str().unwrap()],
    );
    let sub = main.join("packages/app");
    fs::create_dir_all(&sub).unwrap();

    // `pull` with no file argument uses figmog.toml's default, and a new
    // mirror goes to the shared root inside the Git directory.
    let out = figmog(&sub)
        .args(["pull", "--from-file"])
        .arg(response(dir.path()))
        .assert()
        .success();
    assert_eq!(json(&out.get_output().stdout)["added"], 18);
    assert!(main.join(".git/figmog").join(KEY).join("db").is_dir());
    assert!(!main.join(".figmog").join(KEY).exists());

    for cwd in [&main, &sub, &linked] {
        let out = figmog(cwd).arg("status").assert().success();
        let output = out.get_output();
        assert_eq!(json(&output.stdout)["name"], "Fixture", "{cwd:?}");
        assert!(output.stderr.is_empty(), "{cwd:?}");
    }
}

#[test]
fn malformed_figmog_toml_is_a_clear_error_with_its_line() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("figmog.toml"), "[files]\ndefault = oops\n").unwrap();
    let out = figmog(dir.path()).arg("status").assert().failure();
    let error = json(&out.get_output().stderr)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.starts_with("figmog.toml:2: expected a quoted string"),
        "{error}"
    );
}

#[test]
fn local_override_picks_the_default_file() {
    let dir = tempfile::tempdir().unwrap();
    let other = "figmogprojectother12345";
    fs::write(
        dir.path().join("figmog.toml"),
        format!("[files]\ndefault = \"{other}\"\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("figmog.local.toml"),
        format!("[files]\ndefault = \"https://www.figma.com/design/{KEY}/Synthetic\"\n"),
    )
    .unwrap();
    figmog(dir.path())
        .args(["pull", "--from-file"])
        .arg(response(dir.path()))
        .assert()
        .success();
    // Outside Git the mirror root is the project's own `.figmog/`.
    assert!(dir.path().join(".figmog").join(KEY).join("db").is_dir());
    assert!(!dir.path().join(".figmog").join(other).exists());
}

#[test]
fn an_old_mirror_is_used_until_migrated_and_two_copies_are_refused() {
    let (dir, main) = repository();
    let legacy = main.join(".figmog").join(KEY).join("db");
    let shared = main.join(".git/figmog").join(KEY).join("db");
    // An old-layout mirror, as an earlier figmog left it.
    figmog(&main)
        .args(["pull", "--from-file"])
        .arg(response(dir.path()))
        .arg("--db")
        .arg(&legacy)
        .assert()
        .success();
    fs::write(main.join(".figmog/current"), KEY).unwrap();

    // Old only: used in place, with a migration-pending note; nothing
    // is created in the shared root.
    let out = figmog(&main).arg("status").assert().success();
    assert_eq!(json(&out.get_output().stdout)["name"], "Fixture");
    let warning = json(&out.get_output().stderr)["warning"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(warning.starts_with("migration pending"), "{warning}");
    assert!(!shared.exists());

    let out = figmog(&main).arg("migrate").assert().success();
    let migrated = json(&out.get_output().stdout);
    assert_eq!(migrated["migrated"][0]["file"], KEY);
    assert_eq!(migrated["migrated"][0]["resumed"], false);
    assert!(shared.is_dir());
    assert!(!legacy.exists());

    // New only: used silently.
    let out = figmog(&main).arg("status").assert().success();
    assert!(out.get_output().stderr.is_empty());

    // Both: a clear error naming both paths, never a silent pick.
    fs::create_dir_all(&legacy).unwrap();
    let out = figmog(&main).arg("status").assert().failure();
    let error = json(&out.get_output().stderr)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains(&legacy.display().to_string()), "{error}");
    assert!(error.contains(&shared.display().to_string()), "{error}");

    // And migrate refuses to overwrite the shared mirror.
    figmog(&main)
        .args(["pull", "--from-file"])
        .arg(response(dir.path()))
        .arg("--db")
        .arg(&legacy)
        .assert()
        .success();
    let out = figmog(&main).args(["migrate", KEY]).assert().failure();
    let error = json(&out.get_output().stderr)["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains("refusing to migrate"), "{error}");
}
