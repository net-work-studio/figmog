//! Synthetic subprocess checks: only the child shell changes its umask.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn pull(work: &Path, args: &[&str]) {
    let output = run_pull(work, args);
    assert!(output.status.success(), "{output:?}");
}

fn run_pull(work: &Path, args: &[&str]) -> Output {
    std::fs::write(
        work.join("fixture.json"),
        r#"{"name":"Synthetic","version":"1","lastModified":"2026-01-01T00:00:00Z","document":{"id":"0:0","name":"Document","type":"DOCUMENT","children":[]}}"#,
    )
    .unwrap();
    let mut command_args = vec!["pull", "--from-file", "fixture.json"];
    command_args.extend_from_slice(args);
    run_figmog(work, &command_args)
}

fn run_figmog(work: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .env("FIGMA_TOKEN", "")
        .args(["-c", "umask 022; exec \"$@\"", "permission-test"])
        .arg(assert_cmd::cargo::cargo_bin!("figmog"))
        .args(args)
        .current_dir(work)
        .output()
        .unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn standalone_pull_protects_the_default_mirror_under_umask_022() {
    let work = tempfile::tempdir().unwrap();
    std::fs::set_permissions(work.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    pull(work.path(), &["SyntheticKey"]);

    // A private store directory blocks traversal to every journal/table,
    // regardless of the modes fold chooses for individual data files.
    let db = work.path().join(".figmog/SyntheticKey/db");
    assert_eq!(mode(&db), 0o700);
    assert_eq!(mode(&work.path().join(".figmog")), 0o700);
    assert!(std::fs::read_dir(&db).unwrap().next().is_some());
    assert_eq!(mode(work.path()), 0o755);
}

#[test]
fn standalone_pull_protects_an_explicit_db_without_chmodding_existing_ancestors() {
    let work = tempfile::tempdir().unwrap();
    let shared = work.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    let db = shared.join("nested/db");

    pull(work.path(), &["--db", "shared/nested/db"]);

    assert_eq!(mode(&db), 0o700);
    assert_eq!(mode(&shared.join("nested")), 0o700);
    assert_eq!(mode(&shared), 0o755);
    assert!(!work.path().join(".figmog").exists());
}

#[test]
fn standalone_pull_tightens_existing_permissive_mirrors_and_keeps_fresh_private() {
    for explicit in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let db_arg = if explicit {
            vec!["--db", "shared/db"]
        } else {
            vec!["SyntheticKey"]
        };
        pull(work.path(), &db_arg);
        let db = work.path().join(if explicit {
            "shared/db"
        } else {
            ".figmog/SyntheticKey/db"
        });
        // Simulate an older mirror with world-traversable directories.
        // Existing data files need no recursive migration: protecting the
        // directory denies access to their payloads as well as new writes.
        for path in db.ancestors().take_while(|p| *p != work.path()) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        pull(work.path(), &db_arg);
        assert_eq!(mode(&db), 0o700);
        if explicit {
            assert_eq!(mode(&work.path().join("shared")), 0o755);
        } else {
            assert_eq!(mode(&work.path().join(".figmog")), 0o700);
        }

        let mut fresh_args = db_arg;
        fresh_args.push("--fresh");
        pull(work.path(), &fresh_args);
        assert_eq!(mode(&db), 0o700);
    }
}

#[test]
fn standalone_pull_refuses_a_symlink_db_before_writing_or_chmodding_its_target() {
    for spelling in ["db", "db/", "db/."] {
        let work = tempfile::tempdir().unwrap();
        let target = work.path().join("unrelated");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, work.path().join("db")).unwrap();

        let output = run_pull(work.path(), &["--db", spelling]);

        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("protecting mirror directory")
        );
        assert!(output.stdout.is_empty());
        assert_eq!(mode(&target), 0o755);
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}

#[test]
fn standalone_pull_rejects_replaceable_ancestors_without_writing() {
    for existing in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let shared = work.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::os::unix::fs::symlink("shared", work.path().join("alias")).unwrap();
        if existing {
            pull(work.path(), &["--db", "shared/db"]);
            std::fs::write(shared.join("db/sentinel"), b"keep existing contents").unwrap();
        }
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        for (spelling, fresh) in [
            ("shared/db", false),
            ("shared/db", true),
            ("alias/db", false),
            ("shared/../other-db", false),
        ] {
            let mut args = vec!["--db", spelling];
            if fresh {
                args.push("--fresh");
            }
            let output = run_pull(work.path(), &args);
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
            assert!(
                error["error"]
                    .as_str()
                    .unwrap()
                    .contains("unsafe mirror ancestor")
            );
            assert_eq!(mode(&shared), 0o777);
            assert_eq!(shared.join("db").exists(), existing);
            assert!(!work.path().join("other-db").exists());
            if existing {
                assert_eq!(
                    std::fs::read(shared.join("db/sentinel")).unwrap(),
                    b"keep existing contents"
                );
            }
        }
    }
}

#[test]
fn standalone_pull_allows_sticky_parents_and_resolves_safe_ancestor_symlinks() {
    let work = tempfile::tempdir().unwrap();
    let shared = work.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    std::os::unix::fs::symlink("shared", work.path().join("alias")).unwrap();

    pull(work.path(), &["--db", "alias/db/."]);

    assert_eq!(mode(&shared.join("db")), 0o700);
    assert_eq!(
        std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
        0o1777
    );
    assert!(
        std::fs::symlink_metadata(work.path().join("alias"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn serve_without_a_socket_also_protects_an_explicit_store() {
    let work = tempfile::tempdir().unwrap();
    pull(work.path(), &["--db", "db"]);
    let db = work.path().join("db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = run_figmog(
        work.path(),
        &[
            "serve",
            "--db",
            "db",
            "--no-socket",
            "--no-watch",
            "--no-upstream",
        ],
    );

    assert!(output.status.success(), "{output:?}");
    assert_eq!(mode(&db), 0o700);
    assert!(!work.path().join(".figmog").exists());
}
