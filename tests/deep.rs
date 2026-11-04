#![recursion_limit = "256"]

mod common;
#[path = "fixtures/deep_file.rs"]
mod deep_file;

#[test]
fn pulls_500_nested_frames_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("deep.json");
    let db = dir.path().join("db");
    std::fs::write(&response, deep_file::file(500)).unwrap();
    for (field, expected) in [("added", 503), ("unchanged", 503)] {
        let output = common::command()
            .arg("--db")
            .arg(&db)
            .arg("pull")
            .arg("--from-file")
            .arg(&response)
            .assert()
            .success();
        let churn: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(churn[field], expected);
    }
    let output = common::command()
        .arg("--db")
        .arg(&db)
        .args(["get", "2:1"])
        .assert()
        .success();
    let leaf: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(leaf["characters"], "Deep leaf");
}

fn parse_deep(text: &[u8]) -> serde_json::Value {
    use serde::Deserialize;
    let mut de = serde_json::Deserializer::from_slice(text);
    de.disable_recursion_limit();
    let value = serde_json::Value::deserialize(serde_stacker::Deserializer::new(&mut de)).unwrap();
    de.end().unwrap();
    value
}

fn assert_chain(mut tree: serde_json::Value) {
    for id in ["0:0".to_string(), "0:1".to_string()]
        .into_iter()
        .chain((1..=500).map(|i| format!("1:{i}")))
    {
        assert_eq!(tree["id"], id);
        let kids = tree["children"].as_array_mut().unwrap();
        assert_eq!(kids.len(), 1);
        tree = kids.remove(0);
    }
    assert_eq!(tree["id"], "2:1");
    assert_eq!(tree["name"], "Leaf");
    assert!(tree["children"].as_array().unwrap().is_empty());
}

#[test]
fn deep_imports_remain_readable_through_cli_and_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("deep.json");
    let db = dir.path().join("db");
    std::fs::write(&response, deep_file::file(500)).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .arg("pull")
        .arg("--from-file")
        .arg(&response)
        .assert()
        .success();
    for args in [vec!["tree"], vec!["dump", "0:0"]] {
        let output = common::command()
            .arg("--db")
            .arg(&db)
            .args(args)
            .assert()
            .success();
        assert_chain(parse_deep(&output.get_output().stdout));
    }
    for (tool, args) in [("figmog_tree", "{}"), ("figmog_subtree", r#"{"id":"0:0"}"#)] {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}
"#
        );
        let output = common::command()
            .arg("--db")
            .arg(&db)
            .args(["--no-socket", "serve", "--no-watch", "--no-upstream"])
            .write_stdin(request)
            .assert()
            .success();
        let envelope = parse_deep(&output.get_output().stdout);
        assert_eq!(envelope["result"]["isError"], false);
        let content = envelope["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert_chain(parse_deep(content.as_bytes()));
        assert!(envelope["result"].get("structuredContent").is_none());
    }
}

#[test]
fn invalid_deep_imports_preserve_the_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("deep.json");
    let db = dir.path().join("db");
    let good = deep_file::file(500);
    std::fs::write(&response, &good).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .arg("pull")
        .arg("--from-file")
        .arg(&response)
        .assert()
        .success();
    for bad in [
        format!("{good} trailing"),
        good[..good.len() - 1].to_string(),
        good.replace(r#""id":"2:1""#, r#""id":"""#),
    ] {
        std::fs::write(&response, bad).unwrap();
        common::command()
            .arg("--db")
            .arg(&db)
            .args(["pull", "--fresh", "--from-file"])
            .arg(&response)
            .assert()
            .failure();
        let output = common::command()
            .arg("--db")
            .arg(&db)
            .arg("status")
            .assert()
            .success();
        let status: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(status["nodes"], 503);
    }
}

#[test]
fn deep_unknown_properties_and_variable_aliases_survive_import() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("bindings.json");
    let db = dir.path().join("db");
    let mut text = String::from(
        r#"{"name":"Bindings","version":"1","lastModified":"now","document":{"id":"0:0","type":"DOCUMENT","payload":"#,
    );
    text.push_str(&r#"{"layer":["#.repeat(500));
    text.push_str(r#"{"boundVariables":{"value":"#);
    text.push_str(&r#"{"part":["#.repeat(500));
    text.push_str(r#"{"type":"VARIABLE_ALIAS","id":"VariableID:deep"}"#);
    text.push_str(&"]}".repeat(500));
    text.push_str("}}");
    text.push_str(&"]}".repeat(500));
    text.push_str("}}");
    std::fs::write(&response, text).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["pull", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let uses = common::command()
        .arg("--db")
        .arg(&db)
        .args(["uses", "VariableID:deep"])
        .assert()
        .success();
    let uses: serde_json::Value = serde_json::from_slice(&uses.get_output().stdout).unwrap();
    assert_eq!(uses.as_array().unwrap().len(), 1);
    assert_eq!(uses[0]["id"], "0:0");

    let raw = common::command()
        .arg("--db")
        .arg(&db)
        .args(["get", "0:0"])
        .assert()
        .success();
    let mut raw = parse_deep(&raw.get_output().stdout);
    let mut payload = raw["payload"].take();
    for _ in 0..500 {
        payload = payload["layer"][0].take();
    }
    let mut alias = payload["boundVariables"]["value"].take();
    for _ in 0..500 {
        alias = alias["part"][0].take();
    }
    assert_eq!(alias["id"], "VariableID:deep");

    // Projection must also safely dispose of an omitted deep property.
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["dump", "0:0", "--fields", "id"])
        .assert()
        .success();
}
