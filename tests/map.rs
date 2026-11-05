#![recursion_limit = "256"]

//! The Figma → code component map (NWS-2949) end to end on a synthetic
//! project: `fixtures/map/pricing.json` (a "Pricing card" component set
//! with Tier=Free/Pro variants, a TEXT and a BOOLEAN property, two
//! instances — one fully expressed by its properties, one with a text
//! override no property explains — and an instance of an unmapped
//! standalone component) and `fixtures/map/figmog.map.json` (the set
//! mapped to `PricingCard`, plus an entry for a component the file
//! doesn't have).
//!
//! Done when: `spec` emits the mapped component with mapped props, and
//! `coverage` reports the unmapped ones.

mod common;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const KEY: &str = "figmogmapfixture1234";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/map")
}

/// A project (figmog.toml, the map, the PricingCard source file) with the
/// pricing file pulled into its mirror.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("figmog.map.json"),
        root.join("figmog.map.json"),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src/components")).unwrap();
    std::fs::write(
        root.join("src/components/PricingCard.tsx"),
        "export function PricingCard() { return null }\n",
    )
    .unwrap();
    run(
        root,
        &["pull", "--from-file", fixture("pricing.json").as_str()],
    );
    dir
}

fn fixture(name: &str) -> String {
    fixtures().join(name).display().to_string()
}

fn figmog(root: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    common::command()
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--no-socket")
        .args(args)
        .assert()
}

fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    figmog(root, args).success().get_output().stdout.clone()
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

fn child<'a>(spec: &'a Value, id: &str) -> &'a Value {
    spec["node"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

#[test]
fn spec_emits_the_mapped_component_with_mapped_props() {
    let dir = project();
    let root = dir.path();
    let bytes = run(root, &["spec", "1:1", "--no-tailwind"]);
    assert_eq!(
        bytes,
        run(root, &["spec", "1:1", "--no-tailwind"]),
        "repeated specs differ"
    );
    let spec = json_of(&bytes);

    // Every property expressed, the title override explained by its
    // property: covered.
    assert_eq!(
        child(&spec, "1:2")["instance"]["code_component"],
        json!({
            "figma": {"component_set": "5e7000000000000000000000000000000000a001"},
            "path": "src/components/PricingCard.tsx",
            "export": "PricingCard",
            "props": {"badge": true, "tier": "pro", "title": "Pro"},
            "jsx": "<PricingCard badge tier=\"pro\" title=\"Pro\" />",
            "coverage": "full",
            "unmapped_properties": [],
            "unmapped_overrides": [],
            "unknown_props": [],
        })
    );
    // The price text was overridden by hand: no property carries it, so
    // the mapping is partial and says why (the hidden badge is its
    // property's doing).
    assert_eq!(
        child(&spec, "1:3")["instance"]["code_component"],
        json!({
            "figma": {"component_set": "5e7000000000000000000000000000000000a001"},
            "path": "src/components/PricingCard.tsx",
            "export": "PricingCard",
            "props": {"badge": false, "tier": "free", "title": "Free"},
            "jsx": "<PricingCard badge={false} tier=\"free\" title=\"Free\" />",
            "coverage": "partial",
            "unmapped_properties": [],
            "unmapped_overrides": [{"id": "I1:3;10:6", "name": "Price", "field": "characters"}],
            "unknown_props": [],
        })
    );
    // An unmapped component: null, as before the map.
    assert_eq!(
        child(&spec, "1:4")["instance"]["code_component"],
        Value::Null
    );

    // The same map as `figmog_spec`'s `map` argument without a project
    // (--db) gives byte-identical output; without it, every
    // code_component is null.
    let db = dir.path().join("db");
    let db = db.to_str().unwrap();
    common::command()
        .args(["--db", db, "pull", "--from-file", &fixture("pricing.json")])
        .assert()
        .success();
    let map: Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("figmog.map.json")).unwrap())
            .unwrap();
    let call = |args: Value| {
        common::command()
            .args(["--db", db, "call", "figmog_spec", "--no-upstream", "--args"])
            .arg(args.to_string())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    assert_eq!(
        call(json!({"id": "1:1", "tailwind": false, "map": map})),
        bytes
    );
    let plain = json_of(&call(json!({"id": "1:1"})));
    assert_eq!(
        child(&plain, "1:2")["instance"]["code_component"],
        Value::Null
    );
}

#[test]
fn coverage_reports_the_unmapped_ones() {
    let dir = project();
    let root = dir.path();
    let bytes = run(root, &["map", "coverage"]);
    assert_eq!(bytes, run(root, &["map", "coverage"]));
    assert_eq!(
        json_of(&bytes),
        json!({
            "map": {"path": "figmog.map.json", "present": true},
            "summary": {"mapped": 1, "unmapped": 2, "not_in_mirror": 1, "code_files_not_present": 0},
            "mapped": [{
                "figma": {
                    "kind": "component_set",
                    "key": "5e7000000000000000000000000000000000a001",
                    "name": "Pricing card",
                    "node_ids": ["10:1"],
                },
                "code": {"path": "src/components/PricingCard.tsx", "export": "PricingCard"},
                "code_file": "present",
                "unknown_props": [],
            }],
            "unmapped": [
                {"kind": "component", "key": "5e7000000000000000000000000000000000b001", "name": "Badge", "node_id": "12:1"},
                {"kind": "component", "key": "5e7000000000000000000000000000000000c001", "name": "Icon/Star", "node_id": "13:1"},
            ],
            "not_in_mirror": [{
                "figma": {"component": "5e7000000000000000000000000000000000dead"},
                "code": {"path": "src/components/LegacyBanner.tsx", "export": "LegacyBanner"},
            }],
        })
    );

    // A stale prop and a missing code file show up too.
    let mut map: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("figmog.map.json")).unwrap())
            .unwrap();
    map["components"][0]["props"]["Size"] = json!({"prop": "size"});
    std::fs::write(root.join("figmog.map.json"), map.to_string()).unwrap();
    std::fs::remove_file(root.join("src/components/PricingCard.tsx")).unwrap();
    let report = json_of(&run(root, &["map", "coverage"]));
    assert_eq!(report["mapped"][0]["unknown_props"], json!(["Size"]));
    assert_eq!(report["mapped"][0]["code_file"], "missing");
    assert_eq!(report["summary"]["code_files_not_present"], 1);
    // In spec, a map key no instance property has is a warning only.
    let spec = json_of(&run(root, &["spec", "1:2"]));
    let mapped = &spec["node"]["instance"]["code_component"];
    assert_eq!(mapped["unknown_props"], json!(["Size"]));
    assert_eq!(mapped["coverage"], "full");

    // Without a map, everything is unmapped.
    std::fs::remove_file(root.join("figmog.map.json")).unwrap();
    let report = json_of(&run(root, &["map", "coverage"]));
    assert_eq!(report["map"]["present"], false);
    assert_eq!(report["summary"]["unmapped"], 3);
}

#[test]
fn an_invalid_map_fails_with_the_file_and_the_json_path() {
    let dir = project();
    let root = dir.path();
    assert_eq!(
        json_of(&run(root, &["map", "check"])),
        json!({"map": "figmog.map.json", "components": 2})
    );
    let mut map: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("figmog.map.json")).unwrap())
            .unwrap();
    map["components"][0]["code"]["path"] = json!("../outside/Card.tsx");
    std::fs::write(root.join("figmog.map.json"), map.to_string()).unwrap();
    for args in [
        &["map", "check"][..],
        &["map", "coverage"],
        &["spec", "1:1"],
    ] {
        let out = figmog(root, args).failure();
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
        assert!(
            stderr.contains("figmog.map.json: components[0].code.path:")
                && stderr.contains("inside the project"),
            "{args:?}: {stderr}"
        );
    }
    std::fs::write(root.join("figmog.map.json"), "{\"version\": 1,").unwrap();
    let out = figmog(root, &["map", "check"]).failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("figmog.map.json: EOF"), "{stderr}");

    // `map` is the project's: no --db.
    let out = common::command()
        .args(["--db", "x", "map", "check"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("does not take --db"), "{stderr}");
    // No map file: `check` says so.
    std::fs::remove_file(root.join("figmog.map.json")).unwrap();
    let out = figmog(root, &["map", "check"]).failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("no figmog.map.json"), "{stderr}");
}
