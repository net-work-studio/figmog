#![recursion_limit = "256"]

//! `figmog spec <node>` (NWS-2968, NWS-2947) end to end on synthetic
//! files: `fixtures/spec/card.json` (an auto-layout card) and
//! `fixtures/spec/edge.json` (the edge cases: negative spacing, wrap, grid,
//! min/max, constraints and clipping, absolute children, text auto-resize,
//! truncation and mixed runs, an instance, assets, effects), each pulled
//! into a fresh `--db` store with `fixtures/spec/variables.json` (a
//! variables export) imported.
//!
//! `fixtures/spec/card.spec.json` and `fixtures/spec/edge.spec.json` are
//! the golden outputs, and `fixtures/spec/tokens.css` the token export of
//! the same variables (what `packages/react`'s browser test renders the
//! edge spec's CSS with). To regenerate them after an intended change:
//! `FIGMOG_BLESS=1 cargo test --test spec`.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spec")
}

/// A fresh store holding the card file and the variable snapshot.
fn card_store() -> (tempfile::TempDir, String) {
    store("card.json", "variables.json")
}

/// A fresh store holding `file` and the variable snapshot `variables`.
fn store(file: &str, variables: &str) -> (tempfile::TempDir, String) {
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
    run(&["pull", "--from-file", &path(file)]);
    run(&["import-variables", &path(variables)]);
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

/// Every value entry in the spec tree: (node id, entry) — every object
/// with a `raw` key outside the `design` layer and the CSS.
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
    for (key, v) in node.as_object().unwrap() {
        if !["children", "design", "css"].contains(&key.as_str()) {
            walk(id, v, out);
        }
    }
    for child in node["children"].as_array().into_iter().flatten() {
        entries(child, out);
    }
}

/// Every binding in a file subtree: (node id, JSON pointer of the
/// `VARIABLE_ALIAS` relative to that node, variable id).
fn file_bindings(root: &Value) -> BTreeSet<(String, String, String)> {
    fn scan(v: &Value, id: &str, at: &str, out: &mut BTreeSet<(String, String, String)>) {
        match v {
            Value::Object(m) => {
                if m.get("type").and_then(Value::as_str) == Some("VARIABLE_ALIAS") {
                    let var = m["id"].as_str().unwrap().to_string();
                    out.insert((id.to_string(), at.to_string(), var));
                    return;
                }
                for (k, x) in m {
                    if k == "children" {
                        for child in x.as_array().into_iter().flatten() {
                            let cid = child["id"].as_str().unwrap();
                            scan(child, cid, "", out);
                        }
                    } else {
                        let key = k.replace('~', "~0").replace('/', "~1");
                        scan(x, id, &format!("{at}/{key}"), out);
                    }
                }
            }
            Value::Array(xs) => {
                for (i, x) in xs.iter().enumerate() {
                    scan(x, id, &format!("{at}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    scan(root, root["id"].as_str().unwrap(), "", &mut out);
    out
}

/// The node `id` in a file response.
fn file_node<'a>(file: &'a Value, id: &str) -> &'a Value {
    fn find<'a>(n: &'a Value, id: &str) -> Option<&'a Value> {
        if n["id"] == id {
            return Some(n);
        }
        n["children"].as_array()?.iter().find_map(|c| find(c, id))
    }
    find(&file["document"], id).unwrap()
}

/// The issue's done-when line (NWS-2947), for one fixture: every binding
/// in the file — by pointer — appears as a token reference (an entry whose
/// `bound_at` names it) or, where the spec can't use it, is listed under
/// `unsupported` by that pointer; every unbound value is flagged (a token
/// whose value it equals is only ever a `matches`); and each `css` name is
/// one the token export emits.
fn assert_bound_and_flagged(spec: &Value, file: &Value, root: &str, css_export: &str) {
    let mut all = Vec::new();
    entries(&spec["node"], &mut all);
    let covers = |q: &str, p: &str| p == q || p.starts_with(&format!("{q}/"));
    for (node, ptr, var) in file_bindings(file_node(file, root)) {
        let as_token = all.iter().any(|(id, e)| {
            *id == node
                && e["variable_id"] == var.as_str()
                && e["bound_at"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|q| covers(q.as_str().unwrap(), &ptr))
        });
        let listed = spec["unsupported"].as_array().unwrap().iter().any(|u| {
            u["id"] == node.as_str()
                && u["reason"]
                    .as_str()
                    .unwrap()
                    .split(": ")
                    .next()
                    .is_some_and(|q| q.starts_with('/') && covers(q, &ptr))
        });
        assert!(
            as_token || listed,
            "{node} {ptr} ({var}) is neither a token reference nor listed"
        );
    }
    for (id, e) in &all {
        if e.get("variable_id").is_some() {
            assert!(e.get("untokenized").is_none(), "{id}: {e}");
            assert!(e["token"].is_string(), "{id}: {e}");
            assert!(e.get("matches").is_none(), "{id}: {e}");
            assert!(!e["bound_at"].as_array().unwrap().is_empty(), "{id}: {e}");
        } else {
            assert_eq!(
                e["untokenized"], true,
                "{id}: unbound value not flagged: {e}"
            );
        }
        if let Some(name) = e["css"].as_str() {
            assert!(
                css_export.contains(&format!("\n  {name}: ")),
                "{name} not exported"
            );
        }
    }
}

/// Run `figmog spec` twice and through `figmog call figmog_spec`; all
/// three must be byte-identical. Returns the bytes.
fn spec_bytes(db: &str, id: &str, tailwind: bool) -> Vec<u8> {
    let flag = if tailwind {
        "--tailwind"
    } else {
        "--no-tailwind"
    };
    let first = stdout(db, &["spec", id, flag]);
    let second = stdout(db, &["spec", id, flag]);
    assert_eq!(first, second, "repeated specs differ");
    let called = stdout(
        db,
        &[
            "call",
            "figmog_spec",
            "--args",
            &json!({"id": id, "tailwind": tailwind}).to_string(),
            "--no-upstream",
        ],
    );
    assert_eq!(called, first);
    first
}

/// Compare `bytes` with the golden file `name` (rewritten under
/// `FIGMOG_BLESS`).
fn golden(name: &str, bytes: &[u8]) {
    let path = fixtures().join(name);
    if std::env::var_os("FIGMOG_BLESS").is_some() {
        std::fs::write(&path, bytes).unwrap();
    }
    assert_eq!(
        String::from_utf8(bytes.to_vec()).unwrap(),
        std::fs::read_to_string(&path).unwrap(),
        "tests/fixtures/spec/{name} is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
    );
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixtures().join(name)).unwrap()).unwrap()
}

#[test]
fn card_spec_is_byte_identical_and_matches_the_golden_file() {
    let (_dir, db) = card_store();
    // `--db` without a project: no Tailwind hint by default.
    let plain = stdout(&db, &["spec", "1:1"]);
    let bytes = spec_bytes(&db, "1:1", false);
    assert_eq!(plain, bytes);
    golden("card.spec.json", &bytes);
}

#[test]
fn bindings_untokenized_values_and_unsupported_cases() {
    let (_dir, db) = card_store();
    let spec: Value = serde_json::from_slice(&stdout(&db, &["spec", "1:1"])).unwrap();
    let css = String::from_utf8(stdout(&db, &["tokens", "export"])).unwrap();
    assert_bound_and_flagged(&spec, &fixture_json("card.json"), "1:1", &css);
    let tokens = &spec["tokens_snapshot"];
    assert_eq!(tokens["state"], "current");
    assert!(tokens["digest"].as_str().unwrap().starts_with("fnv1a64:"));

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

    // Two layers: the design as recorded, the CSS as figmog reads it — a
    // bound value is its token's var(), an unbound one its raw value.
    let root = &spec["node"];
    assert_eq!(root["design"]["layoutMode"], "VERTICAL");
    assert_eq!(root["design"]["paddingLeft"], 24);
    assert_eq!(root["css"]["gap"], "var(--spacing-4)");
    assert_eq!(root["css"]["padding-left"], "24px");
    assert_eq!(root["css"]["padding-top"], "var(--spacing-4)");
    assert_eq!(root["css"]["position"], "relative");

    // The grid is CSS grid; the absolute ribbon is positioned; the
    // mixed-run caption has its runs.
    let grid = &root["children"][2];
    assert_eq!(grid["id"], "1:6");
    assert_eq!(grid["css"]["display"], "grid");
    assert_eq!(grid["css"]["grid-template-columns"], "1fr 1fr");
    assert_eq!(grid["css"]["column-gap"], "var(--spacing-2)");
    assert_eq!(grid["children"][1]["css"]["grid-column"], "2 / span 1");
    let ribbon = &root["children"][3];
    assert_eq!(ribbon["css"]["position"], "absolute");
    assert_eq!(ribbon["css"]["left"], "280px");
    let caption = &root["children"][4];
    assert_eq!(caption["content"]["runs"][1]["text"], "weekly");
    assert_eq!(caption["content"]["runs"][1]["css"]["font-weight"], "700");

    // What stays untranslatable is listed, sorted, never approximated.
    let unsupported = spec["unsupported"].as_array().unwrap();
    let has = |id: &str, prefix: &str| {
        unsupported
            .iter()
            .any(|u| u["id"] == id && u["reason"].as_str().unwrap().starts_with(prefix))
    };
    assert!(has("1:8", "fills[0]: IMAGE paint"), "{unsupported:?}");
    assert!(has("1:9", "fills[0]: GRADIENT_LINEAR"), "{unsupported:?}");
    assert!(
        has("1:9", "absolute child before in-flow siblings"),
        "{unsupported:?}"
    );
    assert!(ribbon["css"].get("background").is_none());
    assert_eq!(
        root["children"][2]["children"][1]["content"]["assets"][0]["image_ref"],
        "0000000000000000000000000000000000000000"
    );
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

/// NWS-2947's edge cases: golden output with the Tailwind hint, and the
/// done-when line on a second fixture.
#[test]
fn edge_cases_spec() {
    let (_dir, db) = store("edge.json", "edge-variables.json");
    let bytes = spec_bytes(&db, "2:1", true);
    golden("edge.spec.json", &bytes);
    let css = stdout(&db, &["tokens", "export"]);
    golden("tokens.css", &css);
    let spec: Value = serde_json::from_slice(&bytes).unwrap();
    assert_bound_and_flagged(
        &spec,
        &fixture_json("edge.json"),
        "2:1",
        &String::from_utf8(css).unwrap(),
    );
    let node = |id: &str| -> Value {
        fn find(n: &Value, id: &str) -> Option<Value> {
            if n["id"] == id {
                return Some(n.clone());
            }
            n["children"].as_array()?.iter().find_map(|c| find(c, id))
        }
        find(&spec["node"], id).unwrap()
    };
    // Negative spacing: margins, not a gap.
    assert!(node("2:2")["css"].get("gap").is_none());
    assert_eq!(node("2:4")["css"]["margin-left"], "-8px");
    // Wrap: both gaps; the unbound row gap is flagged, matching space/2.
    assert_eq!(node("2:6")["css"]["flex-wrap"], "wrap");
    assert_eq!(node("2:6")["css"]["column-gap"], "var(--spacing-2)");
    assert_eq!(
        node("2:6")["layout"]["wrap_gap"]["matches"][0]["token"],
        "space/2"
    );
    // Grid: tracks, spans, child alignment.
    assert_eq!(node("2:12")["css"]["grid-template-rows"], "48px 48px");
    assert_eq!(node("2:13")["css"]["grid-column"], "1 / span 2");
    assert_eq!(node("2:14")["css"]["justify-self"], "center");
    // Min/max.
    assert_eq!(node("2:17")["css"]["max-width"], "200px");
    assert_eq!(node("2:18")["css"]["min-width"], "120px");
    // Constraints and clipping.
    assert_eq!(node("2:20")["css"]["overflow"], "hidden");
    assert_eq!(node("2:22")["css"]["right"], "8px");
    assert_eq!(node("2:24")["css"]["left"], "calc(50% + 10px)");
    assert_eq!(node("2:25")["css"]["width"], "25%");
    // Text: auto-resize, truncation, mixed runs.
    assert_eq!(node("2:31")["css"]["white-space"], "pre");
    assert_eq!(node("2:34")["css"]["text-overflow"], "ellipsis");
    assert_eq!(node("2:35")["css"]["-webkit-line-clamp"], "2");
    let runs = &node("2:33")["content"]["runs"];
    assert_eq!(runs[1]["text"], "42");
    assert_eq!(
        runs[1]["typography"]["font_weight"]["token"],
        "font/weight/bold"
    );
    // The instance: component, variant, the NWS-2949 hook.
    let button = node("2:40");
    assert_eq!(button["instance"]["component_set"]["name"], "Button");
    assert_eq!(
        button["instance"]["variant"],
        json!({"Size": "Small", "State": "Default"})
    );
    assert_eq!(button["instance"]["code_component"], Value::Null);
    // Assets: the icon by node id, the image by imageRef.
    assert_eq!(
        node("I2:40;3:3")["content"]["assets"][0],
        json!({"kind": "vector", "node_id": "I2:40;3:3", "format": "svg"})
    );
    assert_eq!(node("2:50")["content"]["assets"][0]["kind"], "image");
    // The Tailwind hint: theme utilities for bound tokens only.
    let hint = spec["node"]["tailwind"].as_str().unwrap();
    for class in [
        "flex",
        "flex-col",
        "gap-4",
        "p-4",
        "bg-surface",
        "rounded-md",
    ] {
        assert!(
            hint.split(' ').any(|c| c == class),
            "{class} missing: {hint}"
        );
    }
    // Review findings: a unitless token gets its unit in CSS; a bound size
    // or spacing CSS can't use is still a token reference, and listed; an
    // absolute child of a bordered frame is offset from the padding box;
    // several shadows are listed, not ordered; text keeps its newlines.
    assert_eq!(
        node("2:40")["css"]["padding-left"],
        "calc(var(--space-unitless) * 1px)"
    );
    assert_eq!(
        node("2:50")["css"]["opacity"],
        "calc(var(--opacity-unitless) / 100)"
    );
    assert_eq!(node("2:17")["css"]["opacity"], "var(--opacity-muted)");
    assert_eq!(node("2:23")["layout"]["width"]["token"], "space/4");
    assert_eq!(node("2:23")["css"]["height"], "var(--spacing-2)");
    assert!(node("2:23")["css"].get("width").is_none());
    assert_eq!(node("2:60")["layout"]["item_spacing"]["token"], "space/2");
    assert_eq!(
        node("2:71")["css"]["right"],
        "calc(8px - var(--stroke-thin))"
    );
    assert!(node("2:21")["css"].get("box-shadow").is_none());
    assert_eq!(node("2:36")["css"]["white-space"], "pre-wrap");
    assert_eq!(node("2:31")["css"]["white-space"], "pre");
    let reasons: Vec<(String, String)> = spec["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["id"].as_str().unwrap().into(),
                u["reason"].as_str().unwrap().into(),
            )
        })
        .collect();
    for (id, prefix) in [
        ("2:21", "effects: 2 shadows"),
        ("2:23", "/boundVariables/size/x: width is bound"),
        ("2:60", "/boundVariables/itemSpacing: itemSpacing is bound"),
    ] {
        assert!(
            reasons
                .iter()
                .any(|(i, r)| i == id && r.starts_with(prefix)),
            "{id}: {prefix} not listed: {reasons:?}"
        );
    }
    // The rotated child is the one untranslatable placement.
    assert!(
        spec["unsupported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == "2:26" && u["reason"].as_str().unwrap().starts_with("rotation"))
    );
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

/// NWS-2948 review: `figmog spec` names tokens as the project's token file
/// does — figmog.toml's `[tokens.modes]` — instead of the default mapping,
/// which here fails (a mode named `日本` has no default selector).
#[test]
fn spec_follows_the_projects_token_mode_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut variables: Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("variables.json")).unwrap())
            .unwrap();
    variables["variableCollections"]["VariableCollectionId:2:0"]["modes"][1]["name"] =
        json!("日本");
    let vars_path = root.join("variables.json");
    std::fs::write(&vars_path, variables.to_string()).unwrap();
    let figmog = |args: &[&str]| -> Value {
        let out = common::command()
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("--no-socket")
            .args(args)
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout).unwrap()
    };
    let config = "[files]\ndefault = \"figmogspecfixture123\"\n";
    std::fs::write(root.join("figmog.toml"), config).unwrap();
    figmog(&[
        "pull",
        "--from-file",
        fixtures().join("card.json").to_str().unwrap(),
    ]);
    figmog(&["import-variables", vars_path.to_str().unwrap()]);

    // The default mapping can't place `日本`: no CSS names at all.
    let spec = figmog(&["spec", "1:1"]);
    let unavailable = spec["tokens_snapshot"]["css_unavailable"].as_str().unwrap();
    assert!(unavailable.contains("日本"), "{unavailable}");

    // The project's mapping can, and spec uses it.
    std::fs::write(
        root.join("figmog.toml"),
        format!("{config}[tokens.modes]\n\"Theme/日本\" = '[data-theme=\"dark\"]'\n"),
    )
    .unwrap();
    let spec = figmog(&["spec", "1:1"]);
    assert_eq!(spec["tokens_snapshot"]["css_unavailable"], Value::Null);
    // `[stack] styling` is `tailwind` (the default): the class hint is on
    // in a project (NWS-2947), using the project's theme names, unless
    // `--no-tailwind`.
    let hint = spec["node"]["tailwind"].as_str().unwrap();
    assert!(hint.split(' ').any(|c| c == "bg-surface"), "{hint}");
    let without = figmog(&["spec", "1:1", "--no-tailwind"]);
    assert!(without["node"].get("tailwind").is_none());
    assert_eq!(
        spec["tokens_snapshot"]["theme"],
        json!({"collection": "Theme", "light_mode": "Light", "dark_mode": "日本"})
    );
    let mut all = Vec::new();
    entries(&spec["node"], &mut all);
    assert!(
        all.iter().any(|(_, e)| e["css"] == "--color-brand"),
        "no bound token carries its CSS name"
    );

    // Without a project (--db), `figmog_spec` takes the mapping as its
    // `tokens` argument; omitted, the defaults apply.
    let db = root.join("db");
    let db = db.to_str().unwrap();
    for args in [
        vec![
            "pull",
            "--from-file",
            fixtures().join("card.json").to_str().unwrap(),
        ],
        vec!["import-variables", vars_path.to_str().unwrap()],
    ] {
        common::command()
            .args(["--db", db])
            .args(&args)
            .assert()
            .success();
    }
    let call = |args: &str| -> Value {
        let out = common::command()
            .args([
                "--db",
                db,
                "call",
                "figmog_spec",
                "--no-upstream",
                "--args",
                args,
            ])
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout).unwrap()
    };
    let plain = call(r#"{"id": "1:1"}"#);
    assert!(plain["tokens_snapshot"]["css_unavailable"].is_string());
    assert!(plain["node"].get("tailwind").is_none());
    let mapped = call(r#"{"id": "1:1", "tokens": {"modes": {"Theme/日本": ".night"}}}"#);
    assert_eq!(mapped["tokens_snapshot"]["css_unavailable"], Value::Null);
    let bad = common::command()
        .args(["--db", db, "call", "figmog_spec", "--no-upstream", "--args"])
        .arg(r#"{"id": "1:1", "tokens": {"modes": {"Theme/日本": ".a, .b"}}}"#)
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&bad.get_output().stderr).to_string();
    assert!(stderr.contains("tokens.modes"), "{stderr}");
}

#[test]
fn font_family_hints_match_the_css_string_values() {
    let families = [
        "Foo_Bar",
        "O'Reilly",
        "Space Family",
        "\"Quoted\" Family",
        r"Back\Slash_Pro",
        "Fancy (Alt)_Face",
        "Foo[Bar]_Alt",
        "Ångström_Face",
    ];
    let children: Vec<Value> = families.iter().enumerate().map(|(i, family)| {
        json!({"id": format!("2:{i}"), "name": family, "type": "TEXT", "characters": "Font sample",
            "absoluteBoundingBox": {"x": 0, "y": i * 20, "width": 200, "height": 20},
            "style": {"fontFamily": family}})
    }).collect();
    let file = json!({"name": "Synthetic font names", "version": "1", "lastModified": "2026-01-01T00:00:00Z",
        "document": {"id": "0:0", "type": "DOCUMENT", "name": "Fonts", "children": [
            {"id": "0:1", "type": "CANVAS", "name": "Fonts", "children": [
                {"id": "1:0", "type": "FRAME", "name": "Fonts", "layoutMode": "VERTICAL",
                    "absoluteBoundingBox": {"x": 0, "y": 0, "width": 200, "height": 160}, "children": children}
            ]}
        ]}, "components": {}, "styles": {}});
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fonts.json");
    std::fs::write(&input, serde_json::to_vec(&file).unwrap()).unwrap();
    let db = dir.path().join("db").display().to_string();
    common::command()
        .args(["--db", &db, "pull", "--from-file"])
        .arg(&input)
        .assert()
        .success();
    let output: Value =
        serde_json::from_slice(&stdout(&db, &["spec", "1:0", "--tailwind"])).unwrap();
    let cases: Vec<Value> = output["node"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .zip(families)
        .map(|(node, family)| {
            let hint = node["tailwind"]
                .as_str()
                .unwrap()
                .split_ascii_whitespace()
                .find(|hint| hint.starts_with("[font-family:"))
                .unwrap();
            json!({"family": family, "css": node["css"]["font-family"], "tailwind": hint})
        })
        .collect();
    assert_eq!(cases.len(), families.len());
    let mut bytes = serde_json::to_vec_pretty(&cases).unwrap();
    bytes.push(b'\n');
    golden("font-families.json", &bytes);
}
