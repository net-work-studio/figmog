#![recursion_limit = "256"]

//! `figmog tokens export` (NWS-2967) end to end: a synthetic variable
//! snapshot (`fixtures/tokens/variables.json`: a Light/Dark theme
//! collection, an alias chain `action/hover → action/primary → brand →
//! ocean/500`, a GAP alias of a unitless FLOAT)
//! imported into a fresh `--db` store and exported.
//!
//! `fixtures/tokens/export.css` is the golden output, also consumed by
//! `packages/react`'s browser test (Tailwind v4 compile + Chromium). To
//! regenerate it after an intended change: `FIGMOG_BLESS=1 cargo test
//! --test tokens`.

mod common;

use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tokens")
}

/// A fresh store holding `export` as its imported variable snapshot.
fn imported(export: &Path) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db").display().to_string();
    common::command()
        .args(["--db", &db, "import-variables", export.to_str().unwrap()])
        .assert()
        .success();
    (dir, db)
}

fn export_stdout(db: &str) -> Vec<u8> {
    let out = common::command()
        .args(["--db", db, "tokens", "export"])
        .assert()
        .success();
    out.get_output().stdout.clone()
}

#[test]
fn fixture_exports_byte_identically_and_matches_the_golden_file() {
    let (dir, db) = imported(&fixtures().join("variables.json"));
    let first = export_stdout(&db);
    let second = export_stdout(&db);
    assert_eq!(first, second, "repeated exports differ");

    // --out writes the same bytes and summarizes on stdout.
    let out_path = dir.path().join("tokens.css");
    let summary = common::command()
        .args(["--db", &db, "tokens", "export", "--out"])
        .arg(&out_path)
        .assert()
        .success();
    assert_eq!(std::fs::read(&out_path).unwrap(), first);
    let summary: serde_json::Value = serde_json::from_slice(&summary.get_output().stdout).unwrap();
    assert_eq!(summary["tokens"], 14);
    assert_eq!(
        summary["theme"],
        serde_json::json!({"collection": "Theme", "collection_id": "VariableCollectionId:2:0", "light_mode": "Light", "dark_mode": "Dark"})
    );
    let reported: Vec<&str> = summary["report"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["variable_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        reported,
        ["VariableID:1:6", "VariableID:1:7", "VariableID:1:8"]
    );

    let golden = fixtures().join("export.css");
    if std::env::var_os("FIGMOG_BLESS").is_some() {
        std::fs::write(&golden, &first).unwrap();
    }
    assert_eq!(
        String::from_utf8(first).unwrap(),
        std::fs::read_to_string(&golden).unwrap(),
        "tests/fixtures/tokens/export.css is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
    );
}

#[test]
fn a_name_collision_fails_naming_both_variables() {
    let mut export: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("variables.json")).unwrap())
            .unwrap();
    // `Ocean 500` kebab-cases to `--ocean-500`, like `ocean/500`.
    let mut twin = export["variables"]["VariableID:1:1"].clone();
    twin["id"] = "VariableID:1:99".into();
    twin["name"] = "Ocean 500".into();
    export["variables"]["VariableID:1:99"] = twin;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("collision.json");
    std::fs::write(&path, export.to_string()).unwrap();

    let (_store, db) = imported(&path);
    let out = common::command()
        .args(["--db", &db, "tokens", "export"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(out.get_output().stdout.is_empty());
    let err: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    let msg = err["error"].as_str().unwrap();
    assert_eq!(
        msg,
        "token name collision: `ocean/500` (VariableID:1:1) and `Ocean 500` (VariableID:1:99) both export as `--ocean-500` — rename one or set its WEB code syntax"
    );
}

#[test]
fn a_store_without_a_snapshot_is_a_clear_error() {
    let (_dir, db) = common::fixture_db();
    let out = common::command()
        .args(["--db", db.to_str().unwrap(), "tokens", "export"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("no variable snapshot (state: none)"),
        "{stderr}"
    );
}

#[test]
fn explicit_modes_are_validated() {
    let (_dir, db) = imported(&fixtures().join("variables.json"));
    let out = common::command()
        .args(["--db", &db, "tokens", "export", "--dark-mode", "Night"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains(r#"collection \"Theme\" has no mode named \"Night\""#),
        "{stderr}"
    );
    // Swapping the modes is allowed and changes which mode is :root.
    let out = common::command()
        .args([
            "--db",
            &db,
            "tokens",
            "export",
            "--light-mode",
            "Dark",
            "--dark-mode",
            "Light",
        ])
        .assert()
        .success();
    let css = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let dark = css.split("[data-theme=\"dark\"] {\n").nth(1).unwrap();
    let dark = dark.split('}').next().unwrap();
    assert!(
        dark.contains("  --color-brand: var(--ocean-500);\n"),
        "{css}"
    );
}

#[test]
fn duplicate_collection_names_remain_distinct_after_import() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("duplicate-collections.json");
    let export = serde_json::json!({
        "variableCollections": {
            "C:theme": {"name": "Same", "defaultModeId": "M:light", "modes": [
                {"modeId": "M:light", "name": "Light"}, {"modeId": "M:dark", "name": "Dark"}]},
            "C:other": {"name": "Same", "defaultModeId": "M:value", "modes": [
                {"modeId": "M:value", "name": "Value"}]}
        },
        "variables": {
            "V:theme": {"name": "themed", "resolvedType": "FLOAT", "variableCollectionId": "C:theme",
                "valuesByMode": {"M:light": 16, "M:dark": 24}, "scopes": ["WIDTH_HEIGHT", "GAP"]},
            "V:other": {"name": "other", "resolvedType": "FLOAT", "variableCollectionId": "C:other",
                "valuesByMode": {"M:value": 8}, "scopes": ["GAP"]}
        }
    });
    std::fs::write(&path, export.to_string()).unwrap();
    let (_store, db) = imported(&path);
    let vars = common::command()
        .args(["--db", &db, "vars", "--snapshot"])
        .assert()
        .success();
    let vars: serde_json::Value = serde_json::from_slice(&vars.get_output().stdout).unwrap();
    let themed = vars["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["variable_id"] == "V:theme")
        .unwrap();
    assert_eq!(themed["collection"], "Same");
    assert_eq!(themed["collection_id"], "C:theme");
    assert_eq!(themed["scopes"], serde_json::json!(["GAP", "WIDTH_HEIGHT"]));
    let css = String::from_utf8(export_stdout(&db)).unwrap();
    assert!(css.contains("  --other: 8px;"), "{css}");
    assert!(css.contains("  --themed: 16px;"), "{css}");
    let error = common::command()
        .args(["--db", &db, "tokens", "export", "--collection", "Same"])
        .assert()
        .failure()
        .code(1);
    assert!(
        String::from_utf8_lossy(&error.get_output().stderr)
            .contains("several variable collections are named")
    );
}
