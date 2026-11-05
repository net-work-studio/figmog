#![recursion_limit = "256"]

//! `figmog tokens export | diff | check` (NWS-2967, NWS-2948) end to end:
//! synthetic variable snapshots imported into a fresh `--db` store, or a
//! project's mirror, and exported.
//!
//! - `fixtures/tokens/variables.json`: a Light/Dark theme collection, an
//!   alias chain `action/hover → action/primary → brand → ocean/500`, a GAP
//!   alias of a unitless FLOAT. Golden outputs `export.css` (tailwind) and
//!   `export.plain.css` (css).
//! - `fixtures/tokens/modes.json`: independent Brand × Theme × Density
//!   collections with aliases across them, a dark-only alias cycle and an
//!   alias to a remote library variable. Golden output `modes.css`, with
//!   `Brand/Globex` mapped to `.brand-globex`.
//!
//! `export.css` and `modes.css` are also consumed by `packages/react`'s
//! browser test (Tailwind v4 compile + Chromium). To regenerate the golden
//! files after an intended change: `FIGMOG_BLESS=1 cargo test --test
//! tokens`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

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
        serde_json::json!({"collection": "Theme", "light_mode": "Light", "dark_mode": "Dark"})
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

    assert_golden("export.css", &first);
}

/// Compare `bytes` with `fixtures/tokens/<name>` (rewritten first under
/// `FIGMOG_BLESS=1`).
fn assert_golden(name: &str, bytes: &[u8]) {
    let golden = fixtures().join(name);
    if std::env::var_os("FIGMOG_BLESS").is_some() {
        fs::write(&golden, bytes).unwrap();
    }
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        fs::read_to_string(&golden).unwrap(),
        "tests/fixtures/tokens/{name} is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
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
        "token name collision: `ocean/500` (collection \"Primitives\", VariableID:1:1) and `Ocean 500` (collection \"Primitives\", VariableID:1:99) both export as `--ocean-500` — rename one or set its WEB code syntax"
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
fn plain_css_drops_the_tailwind_theme_and_keeps_every_mode() {
    let (_dir, db) = imported(&fixtures().join("variables.json"));
    let out = common::command()
        .args(["--db", &db, "tokens", "export", "--format", "css"])
        .assert()
        .success();
    let css = out.get_output().stdout.clone();
    assert_golden("export.plain.css", &css);
    let css = String::from_utf8(css).unwrap();
    assert!(!css.contains("@theme"), "{css}");
    // Same custom properties as the Tailwind file, minus its theme blocks.
    let tailwind = fs::read_to_string(fixtures().join("export.css")).unwrap();
    assert!(tailwind.starts_with(&css), "{css}");
    let bad = common::command()
        .args(["--db", &db, "tokens", "export", "--format", "dtcg"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&bad.get_output().stderr).to_string();
    assert!(stderr.contains("unsupported token format"), "{stderr}");
}

#[test]
fn explicit_modes_are_validated() {
    let (_dir, db) = imported(&fixtures().join("variables.json"));
    let out = common::command()
        .args([
            "--db",
            &db,
            "tokens",
            "export",
            "--mode",
            "Theme/Night=.night",
        ])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains(r#"mode mapping \"Theme/Night\" names no collection mode"#),
        "{stderr}"
    );
    for bad in ["Theme/Dark", "Theme/Dark=.a, .b"] {
        let out = common::command()
            .args(["--db", &db, "tokens", "export", "--mode", bad])
            .assert()
            .failure()
            .code(1);
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
        assert!(stderr.contains("--mode"), "{bad}: {stderr}");
    }
    // Swapping the modes is allowed and changes which mode is :root.
    let out = common::command()
        .args([
            "--db",
            &db,
            "tokens",
            "export",
            "--mode",
            "Theme/Dark=:root",
            "--mode",
            r#"Theme/Light=[data-theme="light"]"#,
        ])
        .assert()
        .success();
    let css = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        css.contains(
            r#" * theme: collection "Theme": mode "Dark" -> :root, mode "Light" -> [data-theme="light"]"#
        ),
        "{css}"
    );
    let light = css.split("[data-theme=\"light\"] {\n").nth(1).unwrap();
    let light = light.split('}').next().unwrap();
    assert!(
        light.contains("  --color-brand: var(--ocean-500);\n"),
        "{css}"
    );
}

/// Theme × Brand × Density: independent collections combine, every mode
/// gets its selector, nested modes keep each other, and the dark-only
/// cycle and the remote alias are reported, not emitted.
#[test]
fn independent_collections_combine_and_problems_are_reported() {
    let (dir, db) = imported(&fixtures().join("modes.json"));
    let out_path = dir.path().join("modes.css");
    let summary = common::command()
        .args([
            "--db",
            &db,
            "tokens",
            "export",
            "--mode",
            "Brand/Globex=.brand-globex",
            "--out",
        ])
        .arg(&out_path)
        .assert()
        .success();
    let summary: Value = serde_json::from_slice(&summary.get_output().stdout).unwrap();
    assert_golden("modes.css", &fs::read(&out_path).unwrap());
    assert_eq!(summary["status"], "written");
    // Only Theme has a mode on `[data-theme="dark"]`.
    assert_eq!(
        summary["theme"],
        json!({"collection": "Theme", "light_mode": "Light", "dark_mode": "Dark"})
    );
    assert_eq!(
        summary["modes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["selector"] != ":root")
            .map(|m| format!(
                "{}/{}={}",
                m["collection"].as_str().unwrap(),
                m["mode"].as_str().unwrap(),
                m["selector"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            "Brand/Globex=.brand-globex",
            "Density/Compact=[data-density=\"compact\"]",
            "Theme/Dark=[data-theme=\"dark\"]",
        ]
    );
    let report: Vec<(&str, &str)> = summary["report"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["variable_id"].as_str().unwrap(),
                r["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(report.len(), 4, "{report:?}");
    assert!(report[0].1.contains("ambiguous unit"), "{report:?}");
    assert!(
        report[1]
            .1
            .contains("alias cycle --loop-a -> --loop-b -> --loop-a only in some modes"),
        "{report:?}"
    );
    assert_eq!(report[1].1, report[2].1);
    assert!(
        report[3]
            .1
            .starts_with("aliases VariableID:9f3c1e/7:7, which is not in the variable snapshot"),
        "{report:?}"
    );
    // Rerunning leaves the identical file alone.
    let again = common::command()
        .args([
            "--db",
            &db,
            "tokens",
            "export",
            "--mode",
            "Brand/Globex=.brand-globex",
            "--out",
        ])
        .arg(&out_path)
        .assert()
        .success();
    let again: Value = serde_json::from_slice(&again.get_output().stdout).unwrap();
    assert_eq!(again["status"], "unchanged");
}

fn figmog_in(cwd: &Path) -> assert_cmd::Command {
    let mut command = common::command();
    command
        .current_dir(cwd)
        .arg("--no-socket")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// A project (outside Git, so its mirror lives in `.figmog/`) whose
/// figmog.toml names a token file and maps `Brand/Globex`.
fn project(extra: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("figmog.toml"),
        format!(
            "[files]\ndefault = \"figmogtokensfixture123\"\n\n[stack]\ntokens = \"styles/figmog-tokens.css\"\n{extra}"
        ),
    )
    .unwrap();
    dir
}

fn import_in(cwd: &Path, export: &Path) {
    figmog_in(cwd)
        .args(["import-variables", export.to_str().unwrap()])
        .assert()
        .success();
}

/// The issue's done-when line: a two-mode fixture with an alias chain
/// exports (Tailwind v4 and plain CSS) to figmog.toml's token file; `check`
/// passes on unchanged input and fails on changed input, and `diff` lists
/// what changed.
#[test]
fn project_export_check_and_diff() {
    let dir = project("");
    let root = dir.path();
    import_in(root, &fixtures().join("variables.json"));

    // `check` before any export: the file is missing.
    let missing = figmog_in(root)
        .args(["tokens", "check"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&missing.get_output().stderr).to_string();
    assert!(
        stderr.contains("styles/figmog-tokens.css does not exist"),
        "{stderr}"
    );

    // `export` writes `[stack] tokens` (creating its directory).
    let out = figmog_in(root)
        .args(["tokens", "export"])
        .assert()
        .success();
    let summary: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(summary["status"], "written");
    let token_file = root.join("styles/figmog-tokens.css");
    assert_eq!(
        fs::read_to_string(&token_file).unwrap(),
        fs::read_to_string(fixtures().join("export.css")).unwrap()
    );
    // `--stdout` (or `--out -`) still prints the same CSS.
    for args in [["--stdout", ""], ["--out", "-"]] {
        let args: Vec<&str> = args.into_iter().filter(|a| !a.is_empty()).collect();
        let out = figmog_in(root)
            .args(["tokens", "export"])
            .args(&args)
            .assert()
            .success();
        assert_eq!(
            out.get_output().stdout,
            fs::read(&token_file).unwrap(),
            "{args:?}"
        );
    }

    // Unchanged input: `check` certifies the snapshot, with provenance, and
    // makes no claim about the live Figma file.
    let ok = figmog_in(root).args(["tokens", "check"]).assert().success();
    let ok: Value = serde_json::from_slice(&ok.get_output().stdout).unwrap();
    assert_eq!(ok["status"], "ok");
    assert_eq!(
        ok["result"],
        "styles/figmog-tokens.css matches the last published snapshot"
    );
    assert_eq!(
        ok["snapshot"],
        json!({"source": "file", "format": "bare", "format_version": "1",
               "exported_at": "2026-11-01T12:00:00Z", "digest": "fnv1a64:26cca08d211f42ab"})
    );
    assert!(
        ok["note"]
            .as_str()
            .unwrap()
            .contains("not with the live Figma file"),
        "{ok}"
    );
    let unchanged = figmog_in(root).args(["tokens", "diff"]).assert().success();
    let unchanged: Value = serde_json::from_slice(&unchanged.get_output().stdout).unwrap();
    assert_eq!(unchanged["status"], "up_to_date");
    assert_eq!(unchanged["added"], json!([]));

    // Changed input: a new snapshot with one value changed, one variable
    // removed and one added.
    let mut export: Value =
        serde_json::from_str(&fs::read_to_string(fixtures().join("variables.json")).unwrap())
            .unwrap();
    export["exportedAt"] = json!("2026-11-04T08:00:00Z");
    export["variables"]["VariableID:1:2"]["valuesByMode"]["1:0"] =
        json!({"r": 0.5, "g": 0.75, "b": 1.0, "a": 1.0});
    export["variables"]
        .as_object_mut()
        .unwrap()
        .remove("VariableID:1:5");
    let mut added = export["variables"]["VariableID:1:4"].clone();
    added["id"] = json!("VariableID:1:9");
    added["name"] = json!("ink/100");
    export["variables"]["VariableID:1:9"] = added;
    let changed_path = root.join("changed.json");
    fs::write(&changed_path, export.to_string()).unwrap();
    import_in(root, &changed_path);

    let before = fs::read(&token_file).unwrap();
    let failed = figmog_in(root)
        .args(["tokens", "check"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&failed.get_output().stderr).to_string();
    assert!(
        stderr.contains("styles/figmog-tokens.css does not match the last published snapshot (2 added, 2 removed, 1 changed, header"),
        "{stderr}"
    );
    let report: Value = serde_json::from_slice(&failed.get_output().stdout).unwrap();
    assert_eq!(report["status"], "out_of_date");
    assert_eq!(report["snapshot"]["exported_at"], "2026-11-04T08:00:00Z");
    // `check` never touches the file.
    assert_eq!(fs::read(&token_file).unwrap(), before);

    let d = figmog_in(root).args(["tokens", "diff"]).assert().success();
    let d: Value = serde_json::from_slice(&d.get_output().stdout).unwrap();
    let rows = |k: &str| -> Vec<String> {
        d[k].as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}",
                    r["block"].as_str().unwrap(),
                    r["name"].as_str().unwrap(),
                    r["variable_id"]
                )
            })
            .collect()
    };
    assert_eq!(
        rows("added"),
        [
            "@theme inline --color-ink-100 \"VariableID:1:9\"",
            ":root --ink-100 \"VariableID:1:9\""
        ]
    );
    assert_eq!(
        rows("removed"),
        [":root --radius-md null", "@theme --radius-md null"]
    );
    let changed: Vec<(String, String, String)> = d["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["from"].as_str().unwrap().to_string(),
                r["to"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        changed,
        [(
            "--ocean-300".to_string(),
            "#66b3ff".to_string(),
            "#80bfff".to_string()
        )]
    );
    assert_eq!(d["header_changed"], true);

    // Regenerating makes `check` pass again.
    figmog_in(root)
        .args(["tokens", "export"])
        .assert()
        .success();
    figmog_in(root).args(["tokens", "check"]).assert().success();
}

/// `[tokens] format` and `[tokens.modes]` in figmog.toml drive every
/// subcommand; `--format`/`--mode` override them; figmog.local.toml
/// overrides single keys.
#[test]
fn figmog_toml_sets_the_format_and_the_mode_mapping() {
    let dir = project(
        "\n[tokens]\nformat = \"css\"\n\n[tokens.modes]\n\"Brand/Globex\" = \".brand-globex\"\n",
    );
    let root = dir.path();
    import_in(root, &fixtures().join("modes.json"));
    figmog_in(root)
        .args(["tokens", "export"])
        .assert()
        .success();
    let token_file = root.join("styles/figmog-tokens.css");
    let golden = fs::read_to_string(fixtures().join("modes.css")).unwrap();
    let plain = fs::read_to_string(&token_file).unwrap();
    assert!(!plain.contains("@theme"), "{plain}");
    assert!(golden.starts_with(&plain), "{plain}");
    figmog_in(root).args(["tokens", "check"]).assert().success();
    // The same file is out of date for another format or mapping.
    figmog_in(root)
        .args(["tokens", "check", "--format", "tailwind"])
        .assert()
        .failure()
        .code(1);
    figmog_in(root)
        .args([
            "tokens",
            "check",
            "--mode",
            "Brand/Globex=[data-brand=\"globex\"]",
        ])
        .assert()
        .failure()
        .code(1);

    fs::write(
        root.join("figmog.local.toml"),
        "[tokens]\nformat = \"tailwind\"\n",
    )
    .unwrap();
    figmog_in(root)
        .args(["tokens", "export"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&token_file).unwrap(), golden);
    let settings = figmog_in(root).args(["settings"]).assert().success();
    let settings: Value = serde_json::from_slice(&settings.get_output().stdout).unwrap();
    assert_eq!(
        settings["tokens"],
        json!({"format": "tailwind", "modes": {"Brand/Globex": ".brand-globex"}})
    );

    // A malformed mapping is a figmog.toml error with its line.
    fs::write(
        root.join("figmog.toml"),
        "[files]\ndefault = \"figmogtokensfixture123\"\n[tokens.modes]\n\"Brand/Globex\" = \".a, .b\"\n",
    )
    .unwrap();
    let bad = figmog_in(root)
        .args(["tokens", "export"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&bad.get_output().stderr).to_string();
    assert!(stderr.contains("figmog.toml:4: tokens.modes:"), "{stderr}");
}

#[test]
fn diff_and_check_with_db_need_a_file() {
    let (dir, db) = imported(&fixtures().join("variables.json"));
    let out = common::command()
        .args(["--db", &db, "tokens", "check"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("pass --file"), "{stderr}");
    let ok = common::command()
        .args(["--db", &db, "tokens", "check", "--file"])
        .arg(fixtures().join("export.css"))
        .assert()
        .success();
    let ok: Value = serde_json::from_slice(&ok.get_output().stdout).unwrap();
    assert_eq!(ok["certifies"], "matches the last published snapshot");
    // A hand edit is caught.
    let edited = dir.path().join("edited.css");
    fs::write(
        &edited,
        fs::read_to_string(fixtures().join("export.css"))
            .unwrap()
            .replace("--radius-md: 8px;", "--radius-md: 9px;"),
    )
    .unwrap();
    let failed = common::command()
        .args(["--db", &db, "tokens", "check", "--file"])
        .arg(&edited)
        .assert()
        .failure()
        .code(1);
    let report: Value = serde_json::from_slice(&failed.get_output().stdout).unwrap();
    assert_eq!(report["header_changed"], false);
    assert_eq!(report["changed"].as_array().unwrap().len(), 2);
}
