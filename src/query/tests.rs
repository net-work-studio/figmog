use super::*;

#[test]
fn tree_builders_skip_cycles_but_keep_repeated_noncyclic_paths() {
    use fold::pipeline::{FilterMap, FlatMap, Keyed, Map, terminal};

    type GraphNode = (NodeRec, Vec<String>);
    let dir = tempfile::tempdir().unwrap();
    // A synthetic children index permits a shared descendant as well as cycles;
    // normal imports assign only one parent to each node.
    let mut store = fold::stream::KeyedStream::<String, GraphNode, _>::new(
        dir.path().join("db"),
        (
            Map::new(
                |d: &Keyed<String, GraphNode>| Keyed::new(d.key.clone(), d.val.0.clone()),
                terminal::Table::new("nodes"),
            ),
            FlatMap::new(
                |d: &Keyed<String, GraphNode>| {
                    d.val
                        .1
                        .iter()
                        .enumerate()
                        .map(|(i, id)| Keyed::new(d.key.clone(), (i as u32, id.clone())))
                        .collect::<Vec<_>>()
                },
                terminal::Multimap::new("children"),
            ),
            FilterMap::new(
                |_: &Keyed<String, GraphNode>| None::<Keyed<String, VariableRec>>,
                terminal::Table::new("variables"),
            ),
            FilterMap::new(
                |_: &Keyed<String, GraphNode>| None::<Keyed<String, VariableCollectionRec>>,
                terminal::Table::new("variable_collections"),
            ),
        ),
    );
    let fixture = json!({
        "name":"Cycle fixture", "version":"1", "lastModified":"now",
        "document":{"id":"R", "name":"R", "type":"DOCUMENT"}
    });
    let flattened = crate::flatten::flatten_file(&fixture).unwrap();
    let crate::model::Rec::Node(template) = &flattened.recs[0].1 else {
        panic!("document record");
    };
    store.wtx(|tx| {
        for (id, edges) in [
            ("R", vec!["R", "A", "B"]),
            ("A", vec!["C"]),
            ("B", vec!["C"]),
            ("C", vec!["A", "C", "R"]),
        ] {
            let mut node = template.clone();
            node.id = id.to_string();
            node.name = id.to_string();
            node.node_type = "FRAME".to_string();
            node.raw = json!({"id":id, "name":id, "type":"FRAME"}).to_string();
            tx.upsert(
                &id.to_string(),
                &(node, edges.into_iter().map(str::to_string).collect()),
            );
        }
    });
    let leaf = |id| json!({"id":id, "name":id, "type":"FRAME", "children":[]});
    let mut c = leaf("C");
    c["children"] = json!([leaf("A")]);
    let mut a = leaf("A");
    a["children"] = json!([leaf("C")]);
    let mut b = leaf("B");
    b["children"] = json!([c]);
    let mut expected = leaf("R");
    expected["children"] = json!([a, b]);
    store.rtx(|(nodes, children, variables, collections)| {
        let root = nodes.get(&"R".to_string()).unwrap();
        // Check a bounded traversal first so a regression fails instead of hanging.
        for depth in [Some(8), None] {
            assert_eq!(
                tree_to_json(&build_tree(&nodes, &children, &root, depth)),
                expected
            );
            assert_eq!(
                build_subtree(
                    &nodes,
                    &children,
                    &variables,
                    &collections,
                    &root,
                    depth,
                    None,
                    false,
                )
                .unwrap(),
                expected
            );
        }
        for (depth, kids) in [(0, vec![]), (1, vec![leaf("A"), leaf("B")])] {
            let mut expected = leaf("R");
            expected["children"] = Value::Array(kids);
            assert_eq!(
                tree_to_json(&build_tree(&nodes, &children, &root, Some(depth))),
                expected
            );
            assert_eq!(
                build_subtree(
                    &nodes,
                    &children,
                    &variables,
                    &collections,
                    &root,
                    Some(depth),
                    None,
                    false,
                )
                .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn search_ties_keep_order_and_top_k_after_a_broad_query_on_the_same_thread() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::open_store!(&dir.path().join("db"));
    let children: Vec<Value> = (0..300)
        .map(|i| {
            json!({
                "id": format!("1:{i}"),
                "type": "TEXT",
                "name": if i < 10 { "target" } else { "padding" },
            })
        })
        .collect();
    let fixture = json!({
        "name": "Search ties", "version": "1", "lastModified": "2026-11-04T00:00:00Z",
        "document": {
            "id": "0:0", "name": "Document", "type": "DOCUMENT",
            "children": [{
                "id": "0:1", "name": "Page", "type": "CANVAS", "children": children,
            }],
        },
    });
    crate::store::sync(
        &mut store,
        &Default::default(),
        &crate::flatten::flatten_file(&fixture).unwrap(),
        0,
    );

    store.rtx(|((nodes, children, text, ..), ..)| {
        let run = |query, limit| search(&text, &nodes, &children, query, limit, None).unwrap();
        let before_top = run("target", 3);
        let before_all = run("target", 10);
        // Grow upstream's reused per-thread scratch without changing the mirror.
        assert_eq!(run("target padding", 300).as_array().unwrap().len(), 300);
        let after_top = run("target", 3);
        let after_all = run("target", 10);

        let ids = |rows: &Value| {
            rows.as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].clone())
                .collect::<Vec<_>>()
        };
        let expected_top = vec![json!("1:0"), json!("1:1"), json!("1:2")];
        let expected_all = vec![
            json!("1:0"),
            json!("1:1"),
            json!("1:2"),
            json!("1:3"),
            json!("1:4"),
            json!("1:5"),
            json!("1:6"),
            json!("1:7"),
            json!("1:8"),
            json!("1:9"),
        ];
        assert_eq!(ids(&before_top), expected_top);
        assert_eq!(ids(&after_top), expected_top);
        assert_eq!(ids(&before_all), expected_all);
        assert_eq!(ids(&after_all), expected_all);
        assert_eq!(before_top, after_top);
        assert_eq!(before_all, after_all);
        let score = before_all[0]["score"].as_f64().unwrap();
        assert!(score > 0.0);
        assert!(
            before_all
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["score"] == score)
        );
    });
}

#[test]
fn deep_raw_record_features_remain_available() {
    crate::test_support::small_stack(|| {
        let payload = format!(
            "{}null{}",
            r#"{"layer":["#.repeat(5_000),
            "]}".repeat(5_000)
        );
        let fixture = json!({
            "name": "Raw consumers", "version": "1", "lastModified": "now",
            "document": {"id":"0:0", "type":"DOCUMENT", "payload":"PAYLOAD", "resolved_variables":"PAYLOAD", "opacity":0.75,
                "boundVariables": {"opacity":{"type":"VARIABLE_ALIAS", "id":"VariableID:opacity"},
                    "payload":{"type":"VARIABLE_ALIAS", "id":"VariableID:payload"}},
                "fills":[{"type":"IMAGE", "imageRef":"deep-fill", "payload":"PAYLOAD"}],
                "styles":{"fill":"S:deep"},
                "children":[{"id":"1:1", "type":"COMPONENT_SET", "componentPropertyDefinitions":{
                    "Label":{"type":"TEXT", "defaultValue":"Hello", "payload":"PAYLOAD"}}}]},
            "styles":{"S:deep":{"key":"deep-style", "name":"Deep style", "styleType":"FILL"}},
            "componentSets":{"1:1":{"key":"deep-set", "name":"Deep set"}}
        })
        .to_string()
        .replace("\"PAYLOAD\"", &payload);
        let response = crate::deep_json::from_str(&fixture).unwrap();
        let flattened = crate::flatten::flatten_file(&response).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::open_store!(&dir.path().join("db"));
        crate::store::sync(&mut store, &Default::default(), &flattened, 0);
        let scanned = store.rtx(
            |(
                (nodes, children, _, _, styled_by, _, _),
                comps,
                sets,
                styles_table,
                variables,
                collections,
                meta,
                _,
                _,
                images,
            )| {
                let annotated = OwnedValue(
                    node(
                        &nodes,
                        &children,
                        &variables,
                        &collections,
                        "0:0".to_string(),
                        false,
                        true,
                    )
                    .unwrap(),
                );
                assert_eq!(annotated["resolved_variables"].as_array().unwrap().len(), 2);
                let inferred =
                    crate::vars::infer_from_nodes(flattened.recs.iter().filter_map(|(_, rec)| {
                        match rec {
                            crate::model::Rec::Node(node) => Some(node),
                            _ => None,
                        }
                    }));
                assert_eq!(inferred[0].variable_id, "VariableID:opacity");
                assert_eq!(inferred[0].observed, ["0.75"]);
                assert_eq!(
                    inferred[1].observed.as_slice(),
                    std::slice::from_ref(&payload)
                );
                let inventory = OwnedValue(components(&nodes, &comps, &sets).unwrap());
                assert_eq!(
                    inventory["sets"][0]["property_definitions"]["Label"]["defaultValue"],
                    "Hello"
                );
                assert_eq!(
                    crate::deep_json::to_string(
                        &inventory["sets"][0]["property_definitions"]["Label"]["payload"]
                    )
                    .unwrap(),
                    payload
                );
                let style_values = OwnedValue(
                    styles(
                        &nodes,
                        &styles_table,
                        &styled_by,
                        &variables,
                        &collections,
                        None,
                        true,
                        false,
                    )
                    .unwrap(),
                );
                assert_eq!(style_values[0]["value"][0]["imageRef"], "deep-fill");
                assert_eq!(
                    crate::deep_json::to_string(&style_values[0]["value"][0]["payload"]).unwrap(),
                    payload
                );
                crate::images::scan(&nodes, &meta, &images, &["0:0".to_string()], "png", 1000)
            },
        );
        let items = crate::images::resolve(
            &mut store,
            scanned,
            None,
            &|_| panic!("no network"),
            Some("unused"),
            &["0:0".to_string()],
            "png",
            None,
        );
        assert!(
            items
                .iter()
                .any(|item| item.kind == "fill" && item.subject == "deep-fill")
        );
    });
}

#[test]
fn deep_where_queries_move_values_and_clean_up_all_exits() {
    crate::test_support::small_stack(|| {
        let payload = format!(
            "{}null{}",
            r#"{"layer":["#.repeat(5_000),
            "]}".repeat(5_000)
        );
        let text = json!({
            "name":"Where", "version":"1", "lastModified":"now",
            "document":{"id":"0:0", "type":"DOCUMENT", "payload":"PAYLOAD",
                "equal_cases":{"zero":-0.0,"integer":1,"null":null,
                    "object":{"a":1,"b":2},"array":[true,{"key":1.25}]},
                "children":[{"id":"9:9", "type":"TEXT"}]}
        })
        .to_string()
        .replace("\"PAYLOAD\"", &payload);
        let response = crate::deep_json::from_str(&text).unwrap();
        let mut flattened = crate::flatten::flatten_file(&response).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::open_store!(&dir.path().join("db"));
        crate::store::sync(&mut store, &Default::default(), &flattened, 0);
        store.rtx(|((nodes, children, ..), ..)| {
            for (pointer, equals, expected_count) in [
                ("/id", None, 2),
                ("/missing", None, 0),
                ("/id", Some(json!("absent")), 0),
            ] {
                let rows = OwnedValue(
                    where_(&nodes, &children, pointer, equals.as_ref(), None, None).unwrap(),
                );
                assert_eq!(rows.as_array().unwrap().len(), expected_count);
            }
            for (pointer, equals, count) in [
                ("/equal_cases/zero", json!(0.0), 1),
                ("/equal_cases/integer", json!(1.0), 0),
                ("/equal_cases/null", Value::Null, 1),
                ("/equal_cases/object", json!({"b":2,"a":1}), 1),
                ("/equal_cases/object", json!({"a":1,"c":2}), 0),
                ("/equal_cases/object", json!({"a":1,"b":2,"c":3}), 0),
                ("/equal_cases/array", json!([true,{"key":1.25}]), 1),
                ("/equal_cases/array", json!([true]), 0),
            ] {
                let rows = OwnedValue(
                    where_(&nodes, &children, pointer, Some(&equals), None, None).unwrap(),
                );
                assert_eq!(rows.as_array().unwrap().len(), count, "{pointer}");
            }
            let rows = OwnedValue(where_(&nodes, &children, "/payload", None, None, None).unwrap());
            assert_eq!(rows.as_array().unwrap().len(), 1);
            assert_eq!(
                crate::deep_json::to_string(&rows[0]["value"]).unwrap(),
                payload
            );
            let equals = crate::deep_json::from_str(&payload).unwrap();
            let rows = OwnedValue(
                where_(&nodes, &children, "/payload", Some(&equals), None, None).unwrap(),
            );
            assert_eq!(rows.as_array().unwrap().len(), 1);
            assert!(where_(&nodes, &children, "invalid", Some(&equals), None, None).is_err());
        });
        // A malformed later record must safely dispose of the earlier deep match.
        for (_, rec) in &mut flattened.recs {
            if let crate::model::Rec::Node(node) = rec
                && node.id == "9:9"
            {
                node.raw = "{invalid".to_string();
            }
        }
        crate::store::sync(&mut store, &Default::default(), &flattened, 0);
        store.rtx(
            |((nodes, children, ..), _, _, _, variables, collections, ..)| {
                assert!(where_(&nodes, &children, "/payload", None, None, None).is_err());
                assert!(
                    subtree(
                        &nodes,
                        &children,
                        &variables,
                        &collections,
                        "0:0".to_string(),
                        None,
                        None,
                        false
                    )
                    .is_err()
                );
            },
        );
    });
}
