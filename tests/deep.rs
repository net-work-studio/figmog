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

fn raw_feature_fixture() -> String {
    let payload = format!("{}null{}", r#"{"layer":["#.repeat(500), "]}".repeat(500));
    format!(
        r#"{{"name":"Raw features","version":"1","lastModified":"now","document":{{"id":"0:0","type":"DOCUMENT","payload":{payload},"opacity":0.75,"boundVariables":{{"opacity":{{"type":"VARIABLE_ALIAS","id":"VariableID:deep"}}}},"fills":[{{"type":"IMAGE","imageRef":"deep-fill"}}],"styles":{{"fill":"S:deep"}},"children":[{{"id":"1:1","type":"COMPONENT_SET","componentPropertyDefinitions":{{"Label":{{"type":"TEXT","defaultValue":"Hello","payload":{payload}}}}}}}]}},"styles":{{"S:deep":{{"key":"deep-style","name":"Deep style","styleType":"FILL"}}}},"componentSets":{{"1:1":{{"key":"deep-set","name":"Deep set"}}}}}}"#
    )
}

#[test]
fn unrelated_deep_properties_do_not_hide_variable_observations() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("raw-features.json");
    let db = dir.path().join("db");
    std::fs::write(&response, raw_feature_fixture()).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["pull", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let output = common::command()
        .arg("--db")
        .arg(&db)
        .args(["vars", "VariableID:deep"])
        .assert()
        .success();
    let vars: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(vars.as_array().unwrap().len(), 1);
    assert_eq!(vars[0]["sites"], serde_json::json!([["0:0", "/opacity"]]));
    assert_eq!(vars[0]["observed"], serde_json::json!(["0.75"]));
}

#[test]
fn unrelated_deep_properties_do_not_hide_style_values_or_component_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("raw-features.json");
    let db = dir.path().join("db");
    std::fs::write(&response, raw_feature_fixture()).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["pull", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let styles = common::command()
        .arg("--db")
        .arg(&db)
        .args(["styles", "--values"])
        .assert()
        .success();
    let styles: serde_json::Value = serde_json::from_slice(&styles.get_output().stdout).unwrap();
    assert_eq!(styles[0]["value"][0]["imageRef"], "deep-fill");
    let components = common::command()
        .arg("--db")
        .arg(&db)
        .arg("components")
        .assert()
        .success();
    let mut components = parse_deep(&components.get_output().stdout);
    let mut definitions = components["sets"][0]["property_definitions"].take();
    assert_eq!(definitions["Label"]["defaultValue"], "Hello");
    let mut payload = definitions["Label"]["payload"].take();
    for _ in 0..500 {
        payload = payload["layer"][0].take();
    }
    assert!(payload.is_null());
}

#[test]
fn deep_where_values_remain_readable_through_cli_and_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("raw-features.json");
    let db = dir.path().join("db");
    std::fs::write(&response, raw_feature_fixture()).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["pull", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    for (args, count) in [
        (vec!["where", "--pointer", "/id"], 2),
        (vec!["where", "--pointer", "/missing"], 0),
        (
            vec!["where", "--pointer", "/payload", "--equals", "null"],
            0,
        ),
    ] {
        let output = common::command()
            .arg("--db")
            .arg(&db)
            .args(args)
            .assert()
            .success();
        let rows: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), count);
    }
    let requests = [
        (1, serde_json::json!({"pointer":"/id"})),
        (2, serde_json::json!({"pointer":"/missing"})),
        (3, serde_json::json!({"pointer":"/payload"})),
    ]
    .into_iter()
    .map(|(id, args)| {
        serde_json::json!({"jsonrpc":"2.0", "id":id,
        "method":"tools/call", "params":{"name":"figmog_where", "arguments":args}})
        .to_string()
    })
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    let output = common::command()
        .arg("--db")
        .arg(&db)
        .args(["--no-socket", "serve", "--no-watch", "--no-upstream"])
        .write_stdin(requests)
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let responses: std::collections::BTreeMap<_, _> = stdout
        .lines()
        .map(|line| {
            let envelope: serde_json::Value = serde_json::from_str(line).unwrap();
            (envelope["id"].as_u64().unwrap(), envelope)
        })
        .collect();
    assert_eq!(responses.len(), 3);
    for (id, count) in [(1, 2), (2, 0), (3, 1)] {
        let envelope = &responses[&id];
        assert_eq!(envelope["result"]["isError"], false);
        let mut rows = parse_deep(
            envelope["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .as_bytes(),
        );
        assert_eq!(rows.as_array().unwrap().len(), count);
        if envelope["id"] == 3 {
            let mut value = rows[0]["value"].take();
            for _ in 0..500 {
                value = value["layer"][0].take();
            }
            assert!(value.is_null());
        }
    }
}

#[test]
fn deep_equals_matches_through_direct_cli_and_stdio_serve() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("raw-features.json");
    let db = dir.path().join("db");
    let payload = format!("{}null{}", r#"{"layer":["#.repeat(500), "]}".repeat(500));
    std::fs::write(&response, raw_feature_fixture()).unwrap();
    common::command()
        .arg("--db")
        .arg(&db)
        .args(["pull", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    let direct = common::command()
        .arg("--db")
        .arg(&db)
        .args(["where", "--pointer", "/payload", "--equals", &payload])
        .assert()
        .success();
    assert_deep_equals_match(parse_deep(&direct.get_output().stdout));

    let request = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"figmog_where","arguments":{{"pointer":"/payload","equals":{payload}}}}}}}
"#
    );
    let output = common::command()
        .arg("--db")
        .arg(&db)
        .args(["--no-socket", "serve", "--no-watch", "--no-upstream"])
        .write_stdin(request)
        .assert()
        .success();
    let envelope: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert!(envelope.get("error").is_none(), "{envelope}");
    assert_eq!(envelope["result"]["isError"], false);
    assert_deep_equals_match(parse_deep(
        envelope["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .as_bytes(),
    ));
}

fn assert_deep_equals_match(mut rows: serde_json::Value) {
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], "0:0");
    let mut value = rows[0]["value"].take();
    for _ in 0..500 {
        value = value["layer"][0].take();
    }
    assert!(value.is_null());
}

#[test]
fn deep_equals_survives_current_mirror_routing_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("fixture.json");
    std::fs::write(
        &response,
        serde_json::json!({
            "name": "Routing", "version": "1", "lastModified": "now",
            "document": {"id": "0:0", "type": "DOCUMENT"}
        })
        .to_string(),
    )
    .unwrap();
    common::command()
        .current_dir(dir.path())
        .args(["pull", "SyntheticFileKey123", "--from-file"])
        .arg(&response)
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".figmog/current"))
            .unwrap()
            .trim(),
        "SyntheticFileKey123"
    );
    // Keep this compact deep argument below the OS's per-argument size limit.
    let equals = format!("{}null{}", r#"{"":["#.repeat(15_000), "]}".repeat(15_000));
    for no_socket in [true, false] {
        let mut command = common::command();
        command.current_dir(dir.path());
        if no_socket {
            command.arg("--no-socket");
        }
        let output = command
            .args(["where", "--pointer", "/id", "--equals", &equals])
            .output()
            .expect("launch CLI");
        assert!(
            output.status.success(),
            "{:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows, serde_json::json!([]));
    }
}
