#![recursion_limit = "256"]

//! `figmog spec <node>` (NWS-2968) end to end on a synthetic auto-layout
//! card: `fixtures/spec/card.json` (a Figma file response) pulled into a
//! fresh `--db` store, `fixtures/spec/variables.json` (a variables export)
//! imported, then `spec 1:1`.
//!
//! `fixtures/spec/card.spec.json` is the golden output. To regenerate it
//! after an intended change: `FIGMOG_BLESS=1 cargo test --test spec`.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spec")
}

/// A fresh store holding the card file and the variable snapshot.
fn card_store() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db").display().to_string();
    let run = |args: &[&str]| {
        common::command()
            .args(["--db", &db])
            .args(args)
            .assert()
            .success();
    };
    let path = |name: &str| fixtures().join(name).display().to_string();
    run(&["pull", "--from-file", &path("card.json")]);
    run(&["import-variables", &path("variables.json")]);
    (dir, db)
}

fn stdout(db: &str, args: &[&str]) -> Vec<u8> {
    common::command()
        .args(["--db", db])
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

/// Every value entry in the spec tree: (node id, entry).
fn entries<'a>(node: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    fn walk<'a>(id: &str, v: &'a Value, out: &mut Vec<(String, &'a Value)>) {
        match v {
            Value::Object(m) if m.contains_key("raw") => out.push((id.to_string(), v)),
            Value::Object(m) => m.values().for_each(|x| walk(id, x, out)),
            Value::Array(xs) => xs.iter().for_each(|x| walk(id, x, out)),
            _ => {}
        }
    }
    let id = node["id"].as_str().unwrap();
    walk(id, &node["layout"], out);
    walk(id, &node["values"], out);
    for child in node["children"].as_array().into_iter().flatten() {
        entries(child, out);
    }
}

#[test]
fn card_spec_is_byte_identical_and_matches_the_golden_file() {
    let (_dir, db) = card_store();
    let first = stdout(&db, &["spec", "1:1"]);
    let second = stdout(&db, &["spec", "1:1"]);
    assert_eq!(first, second, "repeated specs differ");
    // The tool form prints the same bytes.
    let called = stdout(
        &db,
        &[
            "call",
            "figmog_spec",
            "--args",
            r#"{"id":"1:1"}"#,
            "--no-upstream",
        ],
    );
    assert_eq!(called, first);

    let golden = fixtures().join("card.spec.json");
    if std::env::var_os("FIGMOG_BLESS").is_some() {
        std::fs::write(&golden, &first).unwrap();
    }
    assert_eq!(
        String::from_utf8(first).unwrap(),
        std::fs::read_to_string(&golden).unwrap(),
        "tests/fixtures/spec/card.spec.json is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
    );
}

#[test]
fn bindings_untokenized_values_and_unsupported_cases() {
    let (_dir, db) = card_store();
    let spec: Value = serde_json::from_slice(&stdout(&db, &["spec", "1:1"])).unwrap();
    let mut all = Vec::new();
    entries(&spec["node"], &mut all);

    // Every bound value in the specified nodes appears as its token: the
    // (node, variable) pairs equal the file's own bindings, minus the
    // mixed-run caption (1:10), which is unsupported.
    let card: Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("card.json")).unwrap())
            .unwrap();
    let mut file_bindings = BTreeSet::new();
    fn scan(v: &Value, id: &str, out: &mut BTreeSet<(String, String)>) {
        match v {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("VARIABLE_ALIAS") {
                    out.insert((id.to_string(), m["id"].as_str().unwrap().to_string()));
                    return;
                }
                let id = m.get("id").and_then(Value::as_str).unwrap_or(id);
                for (k, x) in m {
                    if k != "id" {
                        scan(x, id, out);
                    }
                }
            }
            Value::Array(xs) => xs.iter().for_each(|x| scan(x, id, out)),
            _ => {}
        }
    }
    scan(&card["document"], "", &mut file_bindings);
    file_bindings.retain(|(node, _)| node != "1:10");
    let spec_bindings: BTreeSet<(String, String)> = all
        .iter()
        .filter_map(|(id, e)| Some((id.clone(), e["variable_id"].as_str()?.to_string())))
        .collect();
    assert_eq!(spec_bindings, file_bindings);
    let tokens = &spec["tokens_snapshot"];
    assert_eq!(tokens["state"], "current");
    assert!(tokens["digest"].as_str().unwrap().starts_with("fnv1a64:"));

    // A bound token's CSS name is exactly what `tokens export` emits.
    let css = String::from_utf8(stdout(&db, &["tokens", "export"])).unwrap();
    for (_, e) in &all {
        if e.get("variable_id").is_some() {
            assert!(e.get("untokenized").is_none(), "{e}");
            assert!(e["token"].is_string(), "{e}");
        }
        if let Some(name) = e["css"].as_str() {
            assert!(
                css.contains(&format!("\n  {name}: ")),
                "{name} not exported"
            );
        }
    }

    // The deliberately unbound card padding is flagged, with no match;
    // the meta row's unbound gap names the token it equals.
    let layout = &spec["node"]["layout"];
    assert_eq!(
        layout["padding"]["left"],
        json!({"untokenized": true, "raw": 24})
    );
    assert_eq!(
        spec["node"]["children"][1]["layout"]["gap"],
        json!({"untokenized": true, "raw": 8, "matches": [
            {"token": "space/2", "variable_id": "VariableID:1:8", "css": "--spacing-2"}
        ]})
    );
    assert_eq!(layout["gap"]["token"], "space/4");
    assert_eq!(layout["gap"]["css"], "--spacing-4");

    // The grid child, the absolute ribbon and the mixed-run caption are
    // unsupported, never specified.
    let unsupported = spec["unsupported"].as_array().unwrap();
    let has = |id: &str, prefix: &str| {
        unsupported
            .iter()
            .any(|u| u["id"] == id && u["reason"].as_str().unwrap().starts_with(prefix))
    };
    assert!(has("1:6", "layoutMode GRID"), "{unsupported:?}");
    assert!(has("1:9", "layoutPositioning ABSOLUTE"), "{unsupported:?}");
    assert!(has("1:10", "mixed text runs"), "{unsupported:?}");
    let grid = &spec["node"]["children"][2];
    assert_eq!(grid["id"], "1:6");
    assert!(grid["layout"].get("direction").is_none(), "{grid}");
    for child in grid["children"].as_array().unwrap() {
        assert!(child.get("layout").is_none(), "{child}");
    }
    let sorted: Vec<(String, String)> = unsupported
        .iter()
        .map(|u| {
            (
                u["id"].as_str().unwrap().to_string(),
                u["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut want = sorted.clone();
    want.sort();
    assert_eq!(sorted, want, "unsupported is sorted");
}

#[test]
fn unknown_node_is_the_standard_error() {
    let (_dir, db) = card_store();
    let out = common::command()
        .args(["--db", &db, "spec", "99:99"])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("no node 99:99 in the mirror"), "{stderr}");
}
