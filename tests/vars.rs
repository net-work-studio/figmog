#![recursion_limit = "256"]

mod common;

use figmog::model::{Id, Rec};
use figmog::vars::parse_variables_export;

fn export() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/variables-export.json")).unwrap()
}

#[test]
fn parses_rest_shape() {
    let recs = parse_variables_export(&export()).unwrap();
    // 2 collections then 3 variables, sorted by id
    assert_eq!(recs.len(), 5);
    assert!(matches!(&recs[0].0, Id::VariableCollection(id) if id == "VariableCollectionId:1"));
    let Rec::VariableCollection(c) = &recs[0].1 else {
        panic!()
    };
    assert_eq!(
        c.modes,
        vec![
            ("1:0".to_string(), "light".to_string()),
            ("1:1".to_string(), "dark".to_string())
        ]
    );
    assert_eq!(c.default_mode_id, "1:0");

    let Rec::Variable(v) = &recs[2].1 else {
        panic!()
    };
    assert_eq!(v.id, "VariableID:100");
    assert_eq!(v.resolved_type, "COLOR");
    assert_eq!(v.collection_id, "VariableCollectionId:1");
    // values canonical JSON, sorted by mode id; alias kept as-is
    assert_eq!(v.values_by_mode[0].0, "1:0");
    assert!(v.values_by_mode[1].1.contains("VARIABLE_ALIAS"));
    assert_eq!(v.scopes, vec!["FRAME_FILL", "SHAPE_FILL"]);
}

#[test]
fn accepts_bare_shape_and_is_deterministic() {
    let bare = export()["meta"].clone();
    let a = parse_variables_export(&bare).unwrap();
    let b = parse_variables_export(&export()).unwrap();
    assert_eq!(
        postcard::to_allocvec(&a).unwrap(),
        postcard::to_allocvec(&b).unwrap(),
        "both shapes produce byte-identical records"
    );
}

#[test]
fn garbage_is_a_shape_error() {
    assert!(parse_variables_export(&serde_json::json!({"nope": 1})).is_err());
    assert!(parse_variables_export(&serde_json::json!(null)).is_err());
}

use figmog::flatten::flatten_file;
use figmog::vars::{infer_from_nodes, style_value_from_consumer};

#[test]
fn infers_values_and_sites_from_fixture() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let nodes: Vec<figmog::model::NodeRec> = out
        .recs
        .iter()
        .filter_map(|(_, r)| match r {
            Rec::Node(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let usages = infer_from_nodes(nodes.iter());

    assert_eq!(usages.len(), 2, "two distinct variables bound in fixture");
    let color = usages
        .iter()
        .find(|u| u.variable_id == "VariableID:100")
        .unwrap();
    assert_eq!(
        color.sites,
        vec![("1:1".to_string(), "/fills/0/color".to_string())]
    );
    let observed: serde_json::Value = serde_json::from_str(&color.observed[0]).unwrap();
    assert_eq!(observed["r"], 0.06);

    let pad = usages
        .iter()
        .find(|u| u.variable_id == "VariableID:200")
        .unwrap();
    assert_eq!(pad.observed, vec!["16.0".to_string()]);
}

#[test]
fn style_values_come_from_consumers() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = out
        .recs
        .iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Node(id), Rec::Node(n)) if id == "1:2" => Some(n.clone()),
            _ => None,
        })
        .unwrap();
    let v = style_value_from_consumer("TEXT", &title.raw).unwrap();
    assert_eq!(v["fontSize"], 32.0);
    assert!(
        style_value_from_consumer("FILL", &title.raw).is_none(),
        "no fills on the text node"
    );
}

// ---- NWS-2961: imports replace the previous snapshot, with provenance ----

use serde_json::{Value, json};

fn run_ok(db: &std::path::Path, args: &[&str]) -> Value {
    let out = common::command()
        .args(args)
        .args(["--db", db.to_str().unwrap()])
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).unwrap()
}

fn import(dir: &std::path::Path, db: &std::path::Path, name: &str, body: &[u8]) -> Value {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    run_ok(db, &["import-variables", path.to_str().unwrap()])
}

fn import_err(dir: &std::path::Path, db: &std::path::Path, name: &str, body: &[u8]) {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let out = common::command()
        .args(["import-variables", path.to_str().unwrap()])
        .args(["--db", db.to_str().unwrap()])
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(err.contains("invalid variables file"), "{err}");
}

fn row<'a>(vars: &'a Value, id: &str) -> Option<&'a Value> {
    vars.as_array()
        .unwrap()
        .iter()
        .find(|v| v["variable_id"] == id)
}

#[test]
fn reimport_removes_deleted_token_and_collection() {
    let (dir, db) = common::fixture_db();
    let first = import(
        dir.path(),
        &db,
        "a.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    assert_eq!(first, json!({"imported": 3, "removed": 0}));

    // Delete one token (VariableID:101) — re-import must drop it.
    let mut v = export();
    v["meta"]["variables"]
        .as_object_mut()
        .unwrap()
        .remove("VariableID:101");
    let second = import(dir.path(), &db, "b.json", &serde_json::to_vec(&v).unwrap());
    assert_eq!(second, json!({"imported": 2, "removed": 1}));
    let vars = run_ok(&db, &["vars"]);
    assert!(row(&vars, "VariableID:101").is_none(), "deleted token gone");
    assert_eq!(row(&vars, "VariableID:100").unwrap()["source"], "imported");

    // Delete a whole collection and its only variable. VariableID:200 is
    // still bound in the mirrored file, so it falls back to inference.
    v["meta"]["variables"]
        .as_object_mut()
        .unwrap()
        .remove("VariableID:200");
    v["meta"]["variableCollections"]
        .as_object_mut()
        .unwrap()
        .remove("VariableCollectionId:2");
    let third = import(dir.path(), &db, "c.json", &serde_json::to_vec(&v).unwrap());
    assert_eq!(third, json!({"imported": 1, "removed": 1}));
    let out = run_ok(&db, &["vars", "--snapshot"]);
    assert_eq!(out["snapshot"]["variables"], 1);
    assert_eq!(out["snapshot"]["collections"], 1);
    assert_eq!(
        row(&out["variables"], "VariableID:200").unwrap()["source"],
        "inferred"
    );
}

#[test]
fn invalid_file_leaves_previous_snapshot_untouched() {
    let (dir, db) = common::fixture_db();
    import(
        dir.path(),
        &db,
        "good.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    let before = run_ok(&db, &["vars", "--snapshot"]);
    let status_before = run_ok(&db, &["status"]);

    import_err(dir.path(), &db, "notjson.json", b"{ not json");
    import_err(dir.path(), &db, "shape.json", br#"{"variables": {}}"#);
    import_err(
        dir.path(),
        &db,
        "entry.json",
        br#"{"variables": {"VariableID:1": 7}, "variableCollections": {}}"#,
    );

    assert_eq!(run_ok(&db, &["vars", "--snapshot"]), before);
    assert_eq!(run_ok(&db, &["status"]), status_before);
    assert_eq!(before["snapshot"]["state"], "current");
}

#[test]
fn provenance_and_code_names_are_readable_in_vars() {
    let (dir, db) = common::fixture_db();
    let mut bare = export()["meta"].clone();
    bare["exportedAt"] = json!("2026-10-01T12:00:00Z");
    bare["formatVersion"] = json!(1);
    let bytes = serde_json::to_vec(&bare).unwrap();
    import(dir.path(), &db, "bare.json", &bytes);

    let out = run_ok(&db, &["vars", "--snapshot"]);
    let snap = &out["snapshot"];
    assert_eq!(snap["state"], "current");
    assert_eq!(snap["source"], "file");
    assert_eq!(snap["format"], "bare");
    assert_eq!(snap["format_version"], "1");
    assert_eq!(snap["exported_at"], "2026-10-01T12:00:00Z");
    assert_eq!(
        snap["digest"],
        figmog::vars::snapshot_digest(&bytes),
        "digest covers the raw file bytes"
    );

    let v100 = row(&out["variables"], "VariableID:100").unwrap();
    assert_eq!(
        v100["code_syntax"],
        json!({"ANDROID": "color_surface_primary", "WEB": "--color-surface-primary",
               "iOS": "colorSurfacePrimary"})
    );
    assert_eq!(v100["hidden_from_publishing"], false);
    assert_eq!(v100["remote"], false);
    let v101 = row(&out["variables"], "VariableID:101").unwrap();
    assert_eq!(v101["code_syntax"], json!({}));
    assert_eq!(v101["hidden_from_publishing"], true);

    // The REST shape (no timestamp) records its own format and no time.
    import(
        dir.path(),
        &db,
        "rest.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    let snap = run_ok(&db, &["vars", "--snapshot"])["snapshot"].clone();
    assert_eq!(snap["format"], "rest");
    assert_eq!(snap["exported_at"], Value::Null);
    assert_eq!(snap["format_version"], Value::Null);

    // The MCP tool path answers identically.
    let call = run_ok(
        &db,
        &[
            "call",
            "figmog_vars",
            "--no-upstream",
            "--args",
            r#"{"snapshot": true}"#,
        ],
    );
    assert_eq!(call["snapshot"], snap);
}

#[test]
fn snapshot_states_reported_in_vars_and_status() {
    let (dir, db) = common::fixture_db();
    let state = |db: &std::path::Path| {
        let vars = run_ok(db, &["vars", "--snapshot"])["snapshot"]["state"].clone();
        let status = run_ok(db, &["status"])["variables"]["state"].clone();
        let call =
            run_ok(db, &["call", "figmog_status", "--no-upstream"])["variables"]["state"].clone();
        assert_eq!(vars, status);
        assert_eq!(vars, call);
        vars
    };
    assert_eq!(state(&db), "none");
    // Plain `vars` keeps its array shape.
    assert!(run_ok(&db, &["vars"]).is_array());

    import(
        dir.path(),
        &db,
        "a.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    assert_eq!(state(&db), "current");

    let path = dir.path().join("empty.json");
    std::fs::write(&path, br#"{"variables": {}, "variableCollections": {}}"#).unwrap();
    let empty = run_ok(
        &db,
        &["import-variables", path.to_str().unwrap(), "--allow-empty"],
    );
    assert_eq!(empty, json!({"imported": 0, "removed": 3}));
    assert_eq!(state(&db), "empty");
    let status = run_ok(&db, &["status"]);
    assert_eq!(status["variables"]["source"], "file");
    assert_eq!(status["variables"]["variables"], 0);
}

/// Old stores still open: variables written before the snapshot row
/// existed read as `legacy` (provenance unknown, code names null), and the
/// next import replaces them like any other snapshot.
#[test]
fn legacy_store_without_snapshot_row_reads_as_legacy() {
    let (dir, db) = common::fixture_db();
    {
        let mut st = figmog::open_store!(&db);
        let recs = parse_variables_export(&export()).unwrap();
        st.wtx(|tx| {
            for (id, rec) in &recs {
                tx.upsert(id, rec);
            }
        });
    }
    let out = run_ok(&db, &["vars", "--snapshot"]);
    assert_eq!(out["snapshot"]["state"], "legacy");
    assert_eq!(out["snapshot"]["source"], Value::Null);
    assert_eq!(out["snapshot"]["variables"], 3);
    let v100 = row(&out["variables"], "VariableID:100").unwrap();
    assert_eq!(v100["source"], "imported");
    assert_eq!(v100["code_syntax"], Value::Null);
    assert_eq!(run_ok(&db, &["status"])["variables"]["state"], "legacy");

    let mut v = export();
    v["meta"]["variables"]
        .as_object_mut()
        .unwrap()
        .remove("VariableID:101");
    let out = import(
        dir.path(),
        &db,
        "new.json",
        &serde_json::to_vec(&v).unwrap(),
    );
    assert_eq!(out, json!({"imported": 2, "removed": 1}));
    assert_eq!(
        run_ok(&db, &["vars", "--snapshot"])["snapshot"]["state"],
        "current"
    );
}

#[test]
fn parses_code_names_into_snapshot() {
    let parsed = figmog::vars::parse_variables_snapshot(&export(), "file", "d".into()).unwrap();
    assert_eq!(parsed.recs, parse_variables_export(&export()).unwrap());
    let s = parsed.snapshot;
    assert_eq!(s.format, "rest");
    let ids: Vec<&str> = s.variables.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["VariableID:100", "VariableID:101", "VariableID:200"]);
    assert_eq!(
        s.variables[0].code_syntax,
        vec![
            ("ANDROID".to_string(), "color_surface_primary".to_string()),
            ("WEB".to_string(), "--color-surface-primary".to_string()),
            ("iOS".to_string(), "colorSurfacePrimary".to_string()),
        ]
    );
    assert!(s.variables[1].hidden_from_publishing);
    assert!(!s.variables[2].remote);
}

/// Non-object entries fail `import-variables`' own check only; the parser
/// Enterprise pulls share stays lenient, as before NWS-2961.
#[test]
fn non_object_entries_fail_import_check_but_not_pull_parser() {
    use figmog::vars::{check_import_entries, enterprise_snapshot_recs};
    let bad = json!({"variables": {"V:1": "x"}, "variableCollections": {}});
    assert!(check_import_entries(&bad).is_err());
    assert_eq!(parse_variables_export(&bad).unwrap().len(), 1);
    let bad = json!({"meta": {"variables": {}, "variableCollections": {"C:1": []}}});
    assert!(check_import_entries(&bad).is_err());
    assert_eq!(parse_variables_export(&bad).unwrap().len(), 1);
    assert_eq!(
        enterprise_snapshot_recs(&bad).unwrap().len(),
        2,
        "rec + snapshot row"
    );
    assert!(check_import_entries(&export()).is_ok());
}

/// A file that parses as JSON but holds a non-object variable entry is
/// rejected whole, leaving the previous snapshot byte-identical.
#[test]
fn import_with_non_object_entry_fails_and_keeps_snapshot() {
    let (dir, db) = common::fixture_db();
    import(
        dir.path(),
        &db,
        "good.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    let before = run_ok(&db, &["vars", "--snapshot"]);

    let mut bad = export();
    bad["meta"]["variables"]["VariableID:999"] = json!("not an object");
    import_err(
        dir.path(),
        &db,
        "bad.json",
        &serde_json::to_vec(&bad).unwrap(),
    );

    assert_eq!(run_ok(&db, &["vars", "--snapshot"]), before);
}

/// A store only ever imported into (never pulled) reports its snapshot in
/// status — file fields null — via the CLI and `figmog call`; a truly
/// empty store keeps the "no mirror here" error.
#[test]
fn import_only_store_reports_snapshot_in_status() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty_db");
    let out = common::command()
        .args(["status", "--db", empty.to_str().unwrap()])
        .assert()
        .failure()
        .code(1);
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(err.contains("no mirror here"), "{err}");

    let db = dir.path().join("fresh_db");
    import(
        dir.path(),
        &db,
        "a.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    let status = run_ok(&db, &["status"]);
    assert_eq!(status["name"], Value::Null);
    assert_eq!(status["version"], Value::Null);
    assert_eq!(status["last_modified"], Value::Null);
    assert_eq!(status["synced_at_unix_ms"], Value::Null);
    assert_eq!(status["nodes"], 0);
    assert_eq!(status["variables"]["state"], "current");
    assert_eq!(status["variables"]["source"], "file");
    assert_eq!(status["variables"]["variables"], 3);

    let call = run_ok(&db, &["call", "figmog_status", "--no-upstream"]);
    assert_eq!(call["variables"], status["variables"]);
    assert_eq!(call["name"], Value::Null);
}

#[test]
fn invalid_import_contents_are_rejected_before_opening_store() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("must-not-exist");
    for (pointer, invalid) in [
        ("/meta/variables/VariableID:100", json!({})),
        ("/meta/variables/VariableID:100/name", json!("")),
        ("/meta/variables/VariableID:100/resolvedType", json!(5)),
        (
            "/meta/variables/VariableID:100/variableCollectionId",
            json!("missing"),
        ),
        (
            "/meta/variables/VariableID:100/valuesByMode",
            json!({"unknown-mode": 1}),
        ),
        (
            "/meta/variableCollections/VariableCollectionId:1/name",
            json!(""),
        ),
        (
            "/meta/variableCollections/VariableCollectionId:1/defaultModeId",
            json!("missing"),
        ),
        (
            "/meta/variables/VariableID:100/codeSyntax",
            json!({"WEB": 5}),
        ),
        ("/meta/variables/VariableID:100/codeSyntax", json!([])),
        (
            "/meta/variables/VariableID:100/hiddenFromPublishing",
            json!("false"),
        ),
        ("/meta/variables/VariableID:100/remote", json!(0)),
    ] {
        let mut bad = export();
        *bad.pointer_mut(pointer).unwrap() = invalid;
        import_err(
            dir.path(),
            &db,
            "invalid.json",
            &serde_json::to_vec(&bad).unwrap(),
        );
        assert!(
            !db.exists(),
            "invalid content at {pointer} must not create a store"
        );
    }
}

#[test]
fn empty_import_needs_opt_in_and_dry_run_preserves_snapshot() {
    let (dir, db) = common::fixture_db();
    import(
        dir.path(),
        &db,
        "good.json",
        include_bytes!("fixtures/variables-export.json"),
    );
    let before = run_ok(&db, &["vars", "--snapshot"]);
    let empty = br#"{"variables":{},"variableCollections":{}}"#;
    import_err(dir.path(), &db, "empty.json", empty);
    assert_eq!(run_ok(&db, &["vars", "--snapshot"]), before);

    let path = dir.path().join("empty.json");
    let preview = run_ok(
        &db,
        &["import-variables", path.to_str().unwrap(), "--dry-run"],
    );
    assert_eq!(
        preview,
        json!({"imported": 0, "removed": 3,
        "removed_ids": ["VariableID:100", "VariableID:101", "VariableID:200"]})
    );
    assert_eq!(run_ok(&db, &["vars", "--snapshot"]), before);
    let applied = run_ok(
        &db,
        &["import-variables", path.to_str().unwrap(), "--allow-empty"],
    );
    assert_eq!(applied, json!({"imported": 0, "removed": 3}));
    assert_eq!(
        run_ok(&db, &["vars", "--snapshot"])["snapshot"]["state"],
        "empty"
    );
}

#[test]
fn dry_run_new_store_does_not_create_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new-db");
    let path = dir.path().join("valid.json");
    std::fs::write(&path, include_bytes!("fixtures/variables-export.json")).unwrap();
    assert_eq!(
        run_ok(
            &db,
            &["import-variables", path.to_str().unwrap(), "--dry-run"]
        ),
        json!({"imported": 3, "removed": 0, "removed_ids": []})
    );
    assert!(!db.exists());
}

#[test]
fn unreadable_import_has_invalid_file_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new-db");
    let missing = dir.path().join("missing.json");
    let result = common::command()
        .args([
            "import-variables",
            missing.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .assert()
        .failure();
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr).contains("invalid variables file")
    );
    assert!(!db.exists());
}

/// JSON objects retain their last duplicate key, as documented for imports.
#[test]
fn duplicate_import_ids_use_the_last_entry() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let mut value = export();
    let original = serde_json::to_string(&value["meta"]["variables"]["VariableID:100"]).unwrap();
    value["meta"]["variables"]["VariableID:100"]["name"] = json!("last name");
    let last = serde_json::to_string(&value["meta"]["variables"]["VariableID:100"]).unwrap();
    let body = serde_json::to_string(&value).unwrap().replace(
        &format!("\"VariableID:100\":{last}"),
        &format!("\"VariableID:100\":{original},\"VariableID:100\":{last}"),
    );
    import(dir.path(), &db, "duplicate.json", body.as_bytes());
    assert_eq!(
        row(&run_ok(&db, &["vars"]), "VariableID:100").unwrap()["name"],
        "last name"
    );
}
