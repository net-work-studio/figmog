#![recursion_limit = "256"]

mod common;

use figmog::flatten::flatten_file;
use figmog::model::{ComponentRec, Id, NodeRec, Rec};

fn node(recs: &[(Id, Rec)], id: &str) -> figmog::model::NodeRec {
    recs.iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Node(n), Rec::Node(rec)) if n == id => Some(rec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("node {id} not flattened"))
}

fn component(recs: &[(Id, Rec)], id: &str) -> ComponentRec {
    recs.iter()
        .find_map(|(k, r)| match (k, r) {
            (Id::Component(n), Rec::Component(rec)) if n == id => Some(rec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("component {id} not flattened"))
}

#[test]
fn walks_the_whole_tree() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let node_ids: Vec<&str> = out
        .recs
        .iter()
        .filter_map(|(k, _)| match k {
            Id::Node(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        node_ids,
        [
            "0:0", "0:1", "1:1", "1:2", "1:3", "1:9", "0:2", "2:1", "2:2", "2:3", "3:1", "0:3"
        ],
        "depth-first order, all 12 nodes"
    );
    assert_eq!(out.file.name, "Fixture");
    assert_eq!(out.file.version, "100");
    assert_eq!(out.file.last_modified, "2026-08-01T00:00:00Z");
}

#[test]
fn parent_index_page_attribution() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = node(&out.recs, "1:2");
    assert_eq!(title.parent_id.as_deref(), Some("1:1"));
    assert_eq!(title.child_index, 0);
    assert_eq!(title.page_id, "0:1");
    let button = node(&out.recs, "1:3");
    assert_eq!(button.child_index, 1);

    let root = node(&out.recs, "0:0");
    assert_eq!(root.parent_id, None);
    assert_eq!(root.page_id, "0:0");
    let canvas = node(&out.recs, "0:2");
    assert_eq!(canvas.page_id, "0:2");
    let variant = node(&out.recs, "2:2");
    assert_eq!(variant.page_id, "0:2");
}

#[test]
fn basic_fields() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let title = node(&out.recs, "1:2");
    assert_eq!(title.node_type, "TEXT");
    assert_eq!(title.name, "Title");
    assert!(title.visible);
    assert_eq!(title.text.as_deref(), Some("Welcome to the garden"));

    let hidden = node(&out.recs, "1:9");
    assert!(!hidden.visible);

    let hero = node(&out.recs, "1:1");
    assert_eq!(hero.abs_bounds, Some([0.0, 0.0, 800.0, 400.0]));
}

#[test]
fn raw_is_canonical_and_childless() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let hero = node(&out.recs, "1:1");
    let raw: serde_json::Value = serde_json::from_str(&hero.raw).unwrap();
    assert!(raw.get("children").is_none());
    assert_eq!(raw["name"], "Hero");
    // canonical: re-serializing the parsed value reproduces the string
    assert_eq!(serde_json::to_string(&raw).unwrap(), hero.raw);
}

#[test]
fn nested_nodes_preserve_childless_record_bytes() {
    let response = serde_json::json!({
        "name": "Nested", "version": "1", "lastModified": "2026-01-01T00:00:00Z",
        "document": {
            "id": "doc", "type": "DOCUMENT", "children": [{
                "id": "page", "type": "CANVAS", "children": [{
                    "id": "frame", "type": "FRAME", "paddingLeft": 12,
                    "boundVariables": {"paddingLeft": {"type": "VARIABLE_ALIAS", "id": "var:frame"}},
                    "custom": {"children": [{"value": null}], "enabled": false},
                    "children": [
                        {"id": "leaf", "type": "FRAME", "fills": [{
                            "color": {"r": 0.5},
                            "boundVariables": {"color": {"type": "VARIABLE_ALIAS", "id": "var:leaf"}}
                        }], "children": []}
                    ]
                }]
            }]
        }
    });
    // Literal childless JSON and metadata specify the stored records without
    // reproducing the flattening algorithm. Only the top-level children field
    // is stripped; a same-named field in custom data must survive unchanged.
    let expected = [
        (
            "doc", "DOCUMENT", None, 0, "doc",
            r#"{"id":"doc","type":"DOCUMENT"}"#,
            vec![],
        ),
        (
            "page", "CANVAS", Some("doc"), 0, "page",
            r#"{"id":"page","type":"CANVAS"}"#,
            vec![],
        ),
        (
            "frame", "FRAME", Some("page"), 0, "page",
            r#"{"boundVariables":{"paddingLeft":{"id":"var:frame","type":"VARIABLE_ALIAS"}},"custom":{"children":[{"value":null}],"enabled":false},"id":"frame","paddingLeft":12,"type":"FRAME"}"#,
            vec![("/paddingLeft".to_string(), "var:frame".to_string())],
        ),
        (
            "leaf", "FRAME", Some("frame"), 0, "page",
            r#"{"fills":[{"boundVariables":{"color":{"id":"var:leaf","type":"VARIABLE_ALIAS"}},"color":{"r":0.5}}],"id":"leaf","type":"FRAME"}"#,
            vec![("/fills/0/color".to_string(), "var:leaf".to_string())],
        ),
    ]
    .map(|(id, node_type, parent_id, child_index, page_id, raw, bound_variables)| {
        (
            Id::Node(id.to_string()),
            Rec::Node(NodeRec {
                id: id.to_string(),
                parent_id: parent_id.map(str::to_string),
                child_index,
                page_id: page_id.to_string(),
                node_type: node_type.to_string(),
                name: String::new(),
                visible: true,
                text: None,
                component_id: None,
                component_properties: vec![],
                property_definitions: None,
                style_refs: vec![],
                bound_variables,
                abs_bounds: None,
                raw: raw.to_string(),
            }),
        )
    });
    let out = flatten_file(&response).unwrap();
    assert_eq!(
        postcard::to_allocvec(&out.recs).unwrap(),
        postcard::to_allocvec(expected.as_slice()).unwrap(),
    );
}

#[test]
fn deterministic_bytes() {
    let a = flatten_file(&common::fixture_v1()).unwrap();
    let b = flatten_file(&common::fixture_v1()).unwrap();
    let enc = |f: &figmog::flatten::Flattened| postcard::to_allocvec(&f.recs).unwrap();
    assert_eq!(enc(&a), enc(&b));
}

#[test]
fn missing_document_errors() {
    let mut response = common::fixture_v1();
    response.as_object_mut().unwrap().remove("document");
    assert_eq!(
        flatten_file(&response).unwrap_err().to_string(),
        "missing field: document"
    );
}

#[test]
fn malformed_document_roots_are_rejected() {
    use serde_json::json;

    for document in [
        json!(null),
        json!(false),
        json!(42),
        json!("document"),
        json!([]),
        json!({}),
        json!({"type": "DOCUMENT"}),
        json!({"id": null, "type": "DOCUMENT"}),
        json!({"id": 1, "type": "DOCUMENT"}),
        json!({"id": "", "type": "DOCUMENT"}),
        json!({"id": "0:0"}),
        json!({"id": "0:0", "type": null}),
        json!({"id": "0:0", "type": 1}),
        json!({"id": "0:0", "type": ""}),
        json!({"id": "0:0", "type": "CANVAS"}),
    ] {
        let mut response = common::fixture_v1();
        response["document"] = document.clone();
        let error = flatten_file(&response)
            .expect_err(&format!("must reject document: {document}"))
            .to_string();
        assert!(error.contains("document"), "{error}");
    }
}

#[test]
fn malformed_children_are_rejected_at_any_depth() {
    use serde_json::json;

    for children in [json!(null), json!({}), json!("children"), json!(1)] {
        for pointer in ["/document", "/document/children/0/children/0"] {
            let mut response = common::fixture_v1();
            response.pointer_mut(pointer).unwrap()["children"] = children.clone();
            let error = flatten_file(&response).expect_err("children must be an array");
            assert!(error.to_string().contains("children"), "{error}");
        }
    }

    for child in [
        json!(null),
        json!(false),
        json!(1),
        json!("child"),
        json!([]),
        json!({"type": "FRAME", "children": [{"id": "1:2", "type": "TEXT"}]}),
        json!({"id": 1, "type": "FRAME"}),
        json!({"id": "", "type": "FRAME"}),
        json!({"id": "1:1"}),
        json!({"id": "1:1", "type": null}),
        json!({"id": "1:1", "type": 1}),
        json!({"id": "1:1", "type": ""}),
    ] {
        let mut response = common::fixture_v1();
        response["document"]["children"][0]["children"][0] = child;
        let error = flatten_file(&response).expect_err("must not skip malformed subtrees");
        assert!(
            error
                .to_string()
                .contains("document.children[0].children[0]"),
            "{error}"
        );
    }
}

#[test]
fn empty_documents_and_unknown_descendant_types_remain_valid() {
    use serde_json::json;

    let mut response = json!({
        "name": "Empty", "version": "1", "lastModified": "2026-01-01T00:00:00Z",
        "document": {"id": "0:0", "type": "DOCUMENT"}
    });
    for children in [None, Some(json!([]))] {
        if let Some(children) = children {
            response["document"]["children"] = children;
        }
        let out = flatten_file(&response).unwrap();
        assert_eq!(out.recs.len(), 1);
        assert_eq!(node(&out.recs, "0:0").node_type, "DOCUMENT");
    }

    response["document"]["children"] = json!([{
        "id": "0:1", "type": "CANVAS", "children": [{
            "id": "1:1", "type": "FUTURE_NODE", "custom": {"children": null}
        }]
    }]);
    let out = flatten_file(&response).unwrap();
    assert_eq!(out.recs.len(), 3);
    assert_eq!(node(&out.recs, "1:1").node_type, "FUTURE_NODE");
}

#[test]
fn instance_component_fields() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let button = node(&out.recs, "1:3");
    assert_eq!(button.component_id.as_deref(), Some("2:2"));
    // sorted by property name; values are canonical JSON of the `value` field
    assert_eq!(
        button.component_properties,
        vec![
            ("HasIcon".to_string(), "false".to_string()),
            ("Icon".to_string(), "\"3:1\"".to_string()),
            ("Label".to_string(), "\"Go\"".to_string()),
            ("Size".to_string(), "\"Large\"".to_string()),
            ("State".to_string(), "\"Default\"".to_string()),
        ]
    );
}

#[test]
fn property_definitions_on_set_and_component() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let set = node(&out.recs, "2:1");
    let defs: serde_json::Value =
        serde_json::from_str(set.property_definitions.as_deref().unwrap()).unwrap();
    assert_eq!(
        defs["Size"]["variantOptions"],
        serde_json::json!(["Large", "Small"])
    );
    // standalone component without the field -> None
    assert_eq!(node(&out.recs, "3:1").property_definitions, None);
}

#[test]
fn style_refs_extracted_sorted() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    assert_eq!(
        node(&out.recs, "1:1").style_refs,
        vec![("fill".to_string(), "S:1".to_string())]
    );
    assert_eq!(
        node(&out.recs, "1:2").style_refs,
        vec![("text".to_string(), "S:2".to_string())]
    );
}

#[test]
fn bound_variable_scan_finds_all_depths() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let hero = node(&out.recs, "1:1");
    // sorted by pointer; pointer addresses the RESOLVED value location
    assert_eq!(
        hero.bound_variables,
        vec![
            ("/fills/0/color".to_string(), "VariableID:100".to_string()),
            ("/paddingLeft".to_string(), "VariableID:200".to_string()),
        ]
    );
}

#[test]
fn envelope_maps_flattened() {
    let out = flatten_file(&common::fixture_v1()).unwrap();
    let c = component(&out.recs, "2:2");
    assert_eq!(c.key, "key22");
    assert_eq!(c.component_set_id.as_deref(), Some("2:1"));
    assert!(!c.remote);

    let styles: Vec<figmog::model::StyleRec> = out
        .recs
        .iter()
        .filter_map(|(_, r)| match r {
            Rec::Style(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(styles.len(), 2);
    assert_eq!(styles[0].style_id, "S:1"); // sorted by style id
    assert_eq!(styles[0].style_type, "FILL");

    let sets = out
        .recs
        .iter()
        .filter(|(k, _)| matches!(k, Id::ComponentSet(_)))
        .count();
    assert_eq!(sets, 1);
}
