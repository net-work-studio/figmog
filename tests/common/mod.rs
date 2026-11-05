//! Synthetic Figma file fixtures. Deliberately NOT derived from any real
//! file. Shape mirrors GET /v1/files/:key responses.

use assert_cmd::Command;
use serde_json::{Value, json};

/// 12 nodes over 3 pages: a hero frame with a text, a variant'd button
/// instance, an invisible node, a component set (2 variants), a standalone
/// component, and an empty page. Fill/text styles + variable bindings.
pub fn fixture_v1() -> Value {
    json!({
        "name": "Fixture",
        "version": "100",
        "lastModified": "2026-08-01T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [
                { "id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                    { "id": "1:1", "name": "Hero", "type": "FRAME",
                      "absoluteBoundingBox": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 400.0},
                      "layoutMode": "VERTICAL", "paddingLeft": 16.0,
                      "boundVariables": { "paddingLeft": {"type": "VARIABLE_ALIAS", "id": "VariableID:200"} },
                      "fills": [ { "type": "SOLID",
                                   "color": {"r": 0.06, "g": 0.13, "b": 0.2, "a": 1.0},
                                   "boundVariables": { "color": {"type": "VARIABLE_ALIAS", "id": "VariableID:100"} } } ],
                      "styles": { "fill": "S:1" },
                      "children": [
                        { "id": "1:2", "name": "Title", "type": "TEXT",
                          "characters": "Welcome to the garden",
                          "style": {"fontFamily": "Basis", "fontSize": 32.0, "fontWeight": 500},
                          "styles": { "text": "S:2" },
                          "children": [] },
                        { "id": "1:3", "name": "Button", "type": "INSTANCE",
                          "componentId": "2:2",
                          "componentProperties": {
                              "Size": {"value": "Large", "type": "VARIANT"},
                              "State": {"value": "Default", "type": "VARIANT"},
                              "Label": {"value": "Go", "type": "TEXT"},
                              "HasIcon": {"value": false, "type": "BOOLEAN"},
                              "Icon": {"value": "3:1", "type": "INSTANCE_SWAP"}
                          },
                          "children": [] }
                      ] },
                    { "id": "1:9", "name": "Old badge", "type": "RECTANGLE",
                      "visible": false, "children": [] }
                ] },
                { "id": "0:2", "name": "Components", "type": "CANVAS", "children": [
                    { "id": "2:1", "name": "Button", "type": "COMPONENT_SET",
                      "componentPropertyDefinitions": {
                          "Size": {"type": "VARIANT", "defaultValue": "Large", "variantOptions": ["Large", "Small"]},
                          "State": {"type": "VARIANT", "defaultValue": "Default", "variantOptions": ["Default", "Hover"]},
                          "Label": {"type": "TEXT", "defaultValue": "Go"},
                          "HasIcon": {"type": "BOOLEAN", "defaultValue": false},
                          "Icon": {"type": "INSTANCE_SWAP", "defaultValue": "3:1"}
                      },
                      "children": [
                        { "id": "2:2", "name": "Size=Large, State=Default", "type": "COMPONENT", "children": [] },
                        { "id": "2:3", "name": "Size=Small, State=Hover", "type": "COMPONENT", "children": [] }
                      ] },
                    { "id": "3:1", "name": "IconStar", "type": "COMPONENT", "children": [] }
                ] },
                { "id": "0:3", "name": "Empty", "type": "CANVAS", "children": [] }
            ]
        },
        "components": {
            "2:2": {"key": "key22", "name": "Size=Large, State=Default", "description": "", "componentSetId": "2:1", "remote": false},
            "2:3": {"key": "key23", "name": "Size=Small, State=Hover", "description": "", "componentSetId": "2:1", "remote": false},
            "3:1": {"key": "key31", "name": "IconStar", "description": "a star", "remote": false}
        },
        "componentSets": {
            "2:1": {"key": "keyset21", "name": "Button", "description": "the button", "remote": false}
        },
        "styles": {
            "S:1": {"key": "sk1", "name": "Brand/Primary", "styleType": "FILL", "description": "", "remote": false},
            "S:2": {"key": "sk2", "name": "Heading/H1", "styleType": "TEXT", "description": "", "remote": false}
        }
    })
}

/// v1 plus: rename 1:2, delete 1:9, add 1:4, repoint instance 1:3 at the
/// Small variant, bump version.
#[allow(dead_code)] // not every test binary uses v2
pub fn fixture_v2() -> Value {
    let mut v = fixture_v1();
    v["version"] = json!("101");
    v["lastModified"] = json!("2026-08-02T00:00:00Z");
    let page1 = &mut v["document"]["children"][0];
    // delete 1:9 (second child of the canvas)
    page1["children"].as_array_mut().unwrap().remove(1);
    let hero = &mut page1["children"][0];
    hero["children"][0]["name"] = json!("Headline");
    hero["children"][1]["componentId"] = json!("2:3");
    hero["children"][1]["componentProperties"]["Size"]["value"] = json!("Small");
    hero["children"][1]["componentProperties"]["State"]["value"] = json!("Hover");
    hero["children"].as_array_mut().unwrap().push(json!({
        "id": "1:4", "name": "Subtitle", "type": "TEXT",
        "characters": "Planting season", "children": []
    }));
    v
}

/// A second, small, distinct fixture — used by the multi-file `serve` e2e
/// (`tests/serve.rs`) to prove `file`-argument routing actually reaches a
/// *different* mirror rather than always answering from the first one.
/// Deliberately tiny (3 nodes) and textually disjoint from [`fixture_v1`]:
/// its one TEXT node's `characters` contains "zephyr", a word that appears
/// nowhere in `fixture_v1`, so a search hit for it proves routing.
#[allow(dead_code)] // not every test binary that includes this module calls it
pub fn fixture_other() -> Value {
    json!({
        "name": "OtherFixture",
        "version": "1",
        "lastModified": "2026-08-03T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [
                { "id": "0:1", "name": "Page 1", "type": "CANVAS", "children": [
                    { "id": "1:1", "name": "Banner", "type": "TEXT",
                      "characters": "Feel the zephyr breeze",
                      "children": [] }
                ] }
            ]
        },
        "components": {},
        "componentSets": {},
        "styles": {}
    })
}

/// [`fixture_v1`] with `fillGeometry`/`strokeGeometry` path data added to
/// the Hero frame (`1:1`) — what a real `GET /v1/files/:key?geometry=paths`
/// response carries on vector nodes (v0.0.2 spec §4) but a plain fetch
/// omits. Used to prove `pull --geometry` keeps this data in `raw`; a
/// synthetic stand-in, since the `--from-file` path this crate's tests use
/// never actually issues the network request that decides whether it's
/// present (see `api::file_url`'s own unit tests for that half).
#[allow(dead_code)]
pub fn fixture_with_geometry() -> Value {
    let mut v = fixture_v1();
    let hero = &mut v["document"]["children"][0]["children"][0];
    hero["fillGeometry"] = json!([
        {"path": "M0,0L800,0L800,400L0,400Z", "windingRule": "NONZERO"}
    ]);
    hero["strokeGeometry"] = json!([]);
    v
}

/// Materialize [`fixture_v1`] into a DB via `pull --from-file` and return the
/// (tempdir, db-path) pair every read command — CLI or `serve` — needs.
/// Shared so `tests/cli.rs` and `tests/serve.rs` build the same fixture the
/// same way.
#[allow(dead_code)] // not every test binary that includes this module calls it
pub fn fixture_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(&response, serde_json::to_string(&fixture_v1()).unwrap()).unwrap();
    let db = dir.path().join("db");
    command()
        .args([
            "pull",
            "--from-file",
            response.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .assert()
        .success();
    (dir, db)
}

/// Every subprocess test must be independent of the developer's saved
/// credential and exported token. Empty FIGMA_TOKEN disables both.
#[allow(dead_code)]
pub fn command() -> Command {
    let mut command = Command::cargo_bin("figmog").unwrap();
    command.env("FIGMA_TOKEN", "");
    command
}

/// Capture names, inode identities, and bytes without opening the database.
/// Useful while serve owns the writer lock: a rejected fresh pull must not
/// unlink or modify any of its files or directories.
#[allow(dead_code)]
pub fn store_snapshot(
    path: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, (u64, Vec<u8>)> {
    use std::os::unix::fs::MetadataExt;

    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, (u64, Vec<u8>)>,
    ) {
        let metadata = std::fs::metadata(path).unwrap();
        let bytes = if metadata.is_file() {
            std::fs::read(path).unwrap()
        } else {
            Vec::new()
        };
        out.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            (metadata.ino(), bytes),
        );
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), out);
            }
        }
    }

    let mut out = std::collections::BTreeMap::new();
    visit(path, path, &mut out);
    out
}

/// [`fixture_v1`] plus twelve "garden bed" TEXT nodes on the Empty page, so
/// `search garden` has more hits than the default limit of 10 — the
/// read-adapter parity tests (NWS-2902) need omission of `limit` to be
/// observable, not just "every hit fits either way". Additional nested,
/// sibling and cross-page nodes make each scope and depth flag observable.
#[allow(dead_code)]
pub fn parity_fixture() -> Value {
    let mut v = fixture_v1();
    let beds: Vec<Value> = (0..12)
        .map(|i| {
            json!({
                "id": format!("4:{i}"), "name": format!("Bed {i}"), "type": "TEXT",
                "characters": format!("garden bed {i}"), "children": [],
            })
        })
        .collect();
    v["document"]["children"][2]["children"] = Value::Array(beds);
    // Depth, equality, page and subtree scopes must each remove results,
    // otherwise dropping one flag in an adapter would still pass parity.
    v["document"]["children"][0]["children"][0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "1:4", "name": "Nested", "type": "FRAME", "children": [
                {"id": "1:5", "name": "Nested text", "type": "TEXT",
                 "characters": "A deeper title", "children": []}
            ]
        }));
    v["document"]["children"][0]["children"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"id": "1:6", "name": "Outside text", "type": "TEXT",
               "characters": "Outside the hero", "children": []}),
            json!({"id": "1:7", "name": "Row", "type": "FRAME",
               "layoutMode": "HORIZONTAL", "children": []}),
        ]);
    v["document"]["children"][1]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "2:4", "name": "Column", "type": "FRAME",
               "layoutMode": "VERTICAL", "children": []}));
    v
}

/// One local read exercised through every adapter (NWS-2902): the direct
/// CLI command, the equivalent local tool name, and the tool arguments a
/// `figmog call`/MCP client would send for the same query. Covers every
/// local read tool with representative defaults, options and omission
/// forms (subtree `fields`, search's default `limit`, `where` with no
/// `equals` vs `equals: null`). Null optionals are covered by parser tests.
#[allow(dead_code)]
pub fn read_parity_cases() -> Vec<(Vec<&'static str>, &'static str, Value)> {
    vec![
        (vec!["status"], "figmog_status", json!({})),
        (vec!["pages"], "figmog_pages", json!({})),
        (vec!["tree"], "figmog_tree", json!({})),
        (
            vec!["tree", "1:1", "--depth", "1"],
            "figmog_tree",
            json!({"id": "1:1", "depth": 1}),
        ),
        (vec!["get", "1:1"], "figmog_node", json!({"id": "1:1"})),
        (
            vec!["get", "1:1", "--children", "--resolve-vars"],
            "figmog_node",
            json!({"id": "1:1", "children": true, "resolve_vars": true}),
        ),
        (vec!["dump", "1:1"], "figmog_subtree", json!({"id": "1:1"})),
        (
            vec!["dump", "1:1", "--depth", "1", "--fields", "id,name,fills"],
            "figmog_subtree",
            json!({"id": "1:1", "depth": 1, "fields": ["id", "name", "fills"]}),
        ),
        (
            vec!["dump", "1:1", "--fields", "styles", "--resolve-vars"],
            "figmog_subtree",
            json!({"id": "1:1", "fields": ["styles"], "resolve_vars": true}),
        ),
        (
            vec!["find", "--type", "TEXT"],
            "figmog_find",
            json!({"type": "TEXT"}),
        ),
        (
            vec!["find", "--type", "TEXT", "--page", "0:1"],
            "figmog_find",
            json!({"type": "TEXT", "page": "0:1"}),
        ),
        (
            vec!["find", "--type", "TEXT", "--under", "1:1"],
            "figmog_find",
            json!({"type": "TEXT", "under": "1:1", "page": null}),
        ),
        (
            vec!["search", "garden"],
            "figmog_search",
            json!({"query": "garden"}),
        ),
        (
            vec!["search", "garden", "-n", "3", "--under", "0:3"],
            "figmog_search",
            json!({"query": "garden", "limit": 3, "under": "0:3"}),
        ),
        (
            vec!["instances", "Button"],
            "figmog_instances",
            json!({"target": "Button"}),
        ),
        (vec!["components"], "figmog_components", json!({})),
        (vec!["styles"], "figmog_styles", json!({})),
        (
            vec!["styles", "--type", "TEXT", "--values", "--resolve-vars"],
            "figmog_styles",
            json!({"type": "TEXT", "values": true, "resolve_vars": true}),
        ),
        (vec!["uses", "S:1"], "figmog_uses", json!({"id": "S:1"})),
        (vec!["vars"], "figmog_vars", json!({})),
        (
            vec!["vars", "--snapshot"],
            "figmog_vars",
            json!({"snapshot": true}),
        ),
        (
            vec!["vars", "VariableID:100"],
            "figmog_vars",
            json!({"id": "VariableID:100"}),
        ),
        (vec!["stats"], "figmog_stats", json!({})),
        (vec!["path", "1:2"], "figmog_path", json!({"id": "1:2"})),
        (vec!["text"], "figmog_text", json!({})),
        (
            vec!["text", "--page", "0:1", "--under", "1:1"],
            "figmog_text",
            json!({"page": "0:1", "under": "1:1"}),
        ),
        (
            vec!["where", "--pointer", "/layoutMode"],
            "figmog_where",
            json!({"pointer": "/layoutMode"}),
        ),
        (
            vec!["where", "--pointer", "/layoutMode", "--equals", "null"],
            "figmog_where",
            json!({"pointer": "/layoutMode", "equals": null}),
        ),
        (
            vec![
                "where",
                "--pointer",
                "/layoutMode",
                "--equals",
                "VERTICAL",
                "--page",
                "0:1",
            ],
            "figmog_where",
            json!({"pointer": "/layoutMode", "equals": "VERTICAL", "page": "0:1"}),
        ),
        (
            vec!["at", "--x", "10", "--y", "10"],
            "figmog_at",
            json!({"x": 10, "y": 10}),
        ),
    ]
}
