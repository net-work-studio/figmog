use super::*;

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

// ---- NWS-2942: linear stats/vars/components, byte-identical output ----
//
// `reference` keeps the pre-NWS-2942 implementations verbatim (per-node
// `parent_id` store walks, every node's raw JSON parsed, a component scan
// per set) so the faster versions are checked byte-for-byte against them.
mod reference {
    use super::*;

    fn depth_of<R: Readable>(nodes: &TableReader<'_, R, String, NodeRec>, id: &str) -> usize {
        let mut depth = 0;
        let mut current = id.to_string();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        while let Some(n) = nodes.get(&current) {
            if !visited.insert(current.clone()) {
                break;
            }
            match n.parent_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => break,
            }
        }
        depth
    }

    pub(super) fn max_depth<R: Readable>(nodes: &TableReader<'_, R, String, NodeRec>) -> usize {
        let all: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();
        all.iter()
            .map(|n| depth_of(nodes, &n.id))
            .max()
            .unwrap_or(0)
    }

    pub(super) fn infer(nodes: &[NodeRec]) -> Value {
        type VarData = (Vec<(String, String)>, Vec<String>);
        let mut by_var: BTreeMap<String, VarData> = BTreeMap::new();
        for node in nodes {
            let raw: Value = match serde_json::from_str(&node.raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for (pointer, var_id) in &node.bound_variables {
                let entry = by_var.entry(var_id.clone()).or_default();
                entry.0.push((node.id.clone(), pointer.clone()));
                if let Some(v) = raw.pointer(pointer) {
                    entry.1.push(serde_json::to_string(v).unwrap());
                }
            }
        }
        let rows: Vec<Value> = by_var
            .into_iter()
            .map(|(variable_id, (mut sites, mut observed))| {
                sites.sort();
                observed.sort();
                observed.dedup();
                json!({"variable_id": variable_id, "sites": sites, "observed": observed})
            })
            .collect();
        Value::Array(rows)
    }

    pub(super) fn components<R: Readable>(
        nodes: &TableReader<'_, R, String, NodeRec>,
        components: &TableReader<'_, R, String, ComponentRec>,
        component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    ) -> Value {
        let mut sets: Vec<(String, ComponentSetRec)> = component_sets.iter().collect();
        sets.sort_by(|a, b| a.0.cmp(&b.0));
        let mut all_components: Vec<(String, ComponentRec)> = components.iter().collect();
        all_components.sort_by(|a, b| a.0.cmp(&b.0));
        let sets_json: Vec<Value> = sets
            .iter()
            .map(|(set_id, set)| {
                let variants: Vec<Value> = all_components
                    .iter()
                    .filter(|(_, c)| c.component_set_id.as_deref() == Some(set_id.as_str()))
                    .map(|(cid, c)| json!({"node_id": cid, "name": c.name, "key": c.key}))
                    .collect();
                let property_definitions: Value = nodes
                    .get(set_id)
                    .and_then(|n| n.property_definitions.clone())
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null);
                json!({
                    "node_id": set_id,
                    "name": set.name,
                    "key": set.key,
                    "variants": variants,
                    "property_definitions": property_definitions,
                })
            })
            .collect();
        let standalone: Vec<Value> = all_components
            .iter()
            .filter(|(_, c)| c.component_set_id.is_none())
            .map(|(cid, c)| json!({"node_id": cid, "name": c.name, "key": c.key}))
            .collect();
        json!({"sets": sets_json, "components": standalone})
    }
}

use crate::flatten::{FileInfo, Flattened};
use crate::model::{Id, Rec};

fn node_rec(id: &str, parent: Option<&str>, node_type: &str, page: &str) -> NodeRec {
    NodeRec {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        child_index: 0,
        page_id: page.to_string(),
        node_type: node_type.to_string(),
        name: format!("n{id}"),
        visible: true,
        text: None,
        component_id: None,
        component_properties: Vec::new(),
        property_definitions: None,
        style_refs: Vec::new(),
        bound_variables: Vec::new(),
        abs_bounds: None,
        raw: json!({"id": id, "type": node_type}).to_string(),
    }
}

fn flattened(recs: Vec<(Id, Rec)>) -> Flattened {
    Flattened {
        recs,
        file: FileInfo {
            name: "Synthetic".to_string(),
            version: "1".to_string(),
            last_modified: "2026-11-05T00:00:00Z".to_string(),
        },
    }
}

fn node_entry(n: NodeRec) -> (Id, Rec) {
    (Id::Node(n.id.clone()), Rec::Node(n))
}

/// A tree of `n` nodes: a `chain`-deep spine (far deeper than 63 levels,
/// NWS-2940) with the rest fanned out pseudo-randomly under earlier nodes,
/// plus corrupted-store shapes — a two-node `parent_id` cycle, a
/// self-parented node, a tail leading into a cycle, and an orphan whose
/// parent is missing — that the depth walk must handle exactly as before.
/// Every 7th node binds a variable (half with unparsable raw JSON every
/// 5th binding), so `vars` sees bound, unbound and broken nodes.
fn synthetic_tree(n: usize, chain: usize) -> Vec<(Id, Rec)> {
    let mut recs = vec![
        node_entry(node_rec("0:0", None, "DOCUMENT", "0:0")),
        node_entry(node_rec("0:1", Some("0:0"), "CANVAS", "0:1")),
    ];
    let mut ids = vec!["0:0".to_string(), "0:1".to_string()];
    let mut seed: u64 = 0x2942;
    for i in 0..n {
        let id = format!("1:{i}");
        let parent = if i < chain {
            ids.last().unwrap().clone()
        } else {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ids[1 + (seed >> 33) as usize % (ids.len() - 1)].clone()
        };
        let ty = ["FRAME", "TEXT", "RECTANGLE", "GROUP"][i % 4];
        let mut rec = node_rec(&id, Some(&parent), ty, "0:1");
        if i % 7 == 0 {
            let var = format!("VariableID:{}", i % 13);
            rec.bound_variables = vec![("/fills/0/color".to_string(), var)];
            rec.raw = if i % 35 == 0 {
                "{not json".to_string()
            } else {
                json!({"id": id, "fills": [{"color": {"r": (i % 3) as f64 / 2.0}}]}).to_string()
            };
        }
        ids.push(id);
        recs.push(node_entry(rec));
    }
    for (id, parent) in [
        ("9:0", "9:1"),
        ("9:1", "9:0"),
        ("9:2", "9:2"),
        ("9:3", "9:4"),
        ("9:4", "9:0"),
        ("9:5", "9:missing"),
        ("9:6", "9:5"),
    ] {
        recs.push(node_entry(node_rec(id, Some(parent), "FRAME", "0:1")));
    }
    recs
}

/// `sets` component sets of `per_set` variants each, a matching number of
/// standalone components, and variants pointing at a missing set; every
/// set node carries `property_definitions`.
fn synthetic_library(sets: usize, per_set: usize) -> Vec<(Id, Rec)> {
    let mut recs = vec![
        node_entry(node_rec("0:0", None, "DOCUMENT", "0:0")),
        node_entry(node_rec("0:1", Some("0:0"), "CANVAS", "0:1")),
    ];
    let component = |cid: String, set: Option<String>| {
        (
            Id::Component(cid.clone()),
            Rec::Component(ComponentRec {
                node_id: cid.clone(),
                key: format!("key-{cid}"),
                name: format!("Variant {cid}"),
                description: String::new(),
                component_set_id: set,
                remote: false,
            }),
        )
    };
    for s in 0..sets {
        // Ids are inserted in reverse so the variants' id order differs
        // from insertion order.
        let sid = format!("2:{}", sets - s);
        let mut set_node = node_rec(&sid, Some("0:1"), "COMPONENT_SET", "0:1");
        set_node.property_definitions = Some(json!({"Size": {"type": "VARIANT"}}).to_string());
        recs.push(node_entry(set_node));
        recs.push((
            Id::ComponentSet(sid.clone()),
            Rec::ComponentSet(ComponentSetRec {
                node_id: sid.clone(),
                key: format!("key-{sid}"),
                name: format!("Set {sid}"),
                description: String::new(),
                remote: false,
            }),
        ));
        for v in (0..per_set).rev() {
            recs.push(component(format!("3:{s}-{v}"), Some(sid.clone())));
        }
        recs.push(component(format!("4:{s}"), None));
    }
    recs.push(component("5:0".to_string(), Some("2:missing".to_string())));
    recs
}

#[test]
fn stats_depth_matches_per_node_walks_on_deep_and_corrupted_trees() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::open_store!(&dir.path().join("db"));
    let flat = flattened(synthetic_tree(3_000, 1_500));
    crate::store::sync(&mut store, &Default::default(), &flat, 0);
    store.rtx(
        |((nodes, _, _, _, _, _, by_type), components, sets, styles, variables, ..)| {
            let out = stats(&nodes, &components, &sets, &styles, &variables, &by_type).unwrap();
            let expected = reference::max_depth(&nodes);
            assert!(
                expected > 1_500,
                "the spine is deeper than any recursion limit"
            );
            assert_eq!(out["max_depth"], json!(expected));
        },
    );

    // Corrupted shapes only: the maximum comes from the cycle members and
    // the tail/orphan chains, with no spine to mask them.
    let parents: HashMap<&str, Option<&str>> = [
        ("9:0", Some("9:1")),
        ("9:1", Some("9:0")),
        ("9:2", Some("9:2")),
        ("9:3", Some("9:4")),
        ("9:4", Some("9:0")),
        ("9:5", Some("9:missing")),
        ("9:6", Some("9:5")),
        ("r", None),
        ("c", Some("r")),
    ]
    .into_iter()
    .collect();
    let got: BTreeMap<&str, usize> = depths(&parents).into_iter().collect();
    let want: BTreeMap<&str, usize> = [
        ("9:0", 2),
        ("9:1", 2),
        ("9:2", 1),
        ("9:3", 4),
        ("9:4", 3),
        ("9:5", 1),
        ("9:6", 2),
        ("r", 0),
        ("c", 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
}

#[test]
fn stats_vars_and_components_are_byte_identical_to_the_quadratic_versions() {
    let mut recs = synthetic_tree(2_000, 300);
    recs.extend(synthetic_library(40, 12).into_iter().skip(2));
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::open_store!(&dir.path().join("db"));
    let flat = flattened(recs);
    crate::store::sync(&mut store, &Default::default(), &flat, 0);
    store.rtx(
        |(
            (nodes, _, _, _, _, _, by_type),
            components_t,
            sets,
            styles,
            variables,
            collections,
            _,
            _,
            _,
            _,
            _,
            snapshot,
        )| {
            let got = components(&nodes, &components_t, &sets).unwrap();
            let want = reference::components(&nodes, &components_t, &sets);
            assert_eq!(got.to_string(), want.to_string());
            assert_eq!(got["sets"].as_array().unwrap().len(), 40);

            let got = vars(&nodes, &variables, &collections, &snapshot, None, false).unwrap();
            let all: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();
            let want = reference::infer(&all);
            let want: Vec<Value> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|u| {
                    json!({
                        "variable_id": u["variable_id"],
                        "source": "inferred",
                        "sites": u["sites"],
                        "observed": u["observed"],
                    })
                })
                .collect();
            assert_eq!(got.to_string(), Value::Array(want).to_string());
            assert!(!got.as_array().unwrap().is_empty());

            let got = stats(&nodes, &components_t, &sets, &styles, &variables, &by_type).unwrap();
            assert_eq!(got["max_depth"], json!(reference::max_depth(&nodes)));
        },
    );
}

#[test]
fn flattened_node_count_matches_the_synced_nodes_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::open_store!(&dir.path().join("db"));
    let first = flattened(synthetic_tree(500, 50));
    crate::store::sync(&mut store, &Default::default(), &first, 0);
    macro_rules! count {
        () => {
            store.rtx(|((nodes, ..), ..)| nodes.iter().count())
        };
    }
    assert_eq!(crate::store::flattened_node_count(&first), count!());

    // A smaller re-pull sweeps the vanished nodes: the count follows.
    let prior = store.rtx(|((nodes, ..), components, sets, styles, ..)| {
        crate::store::collect_sweepable(&nodes, &components, &sets, &styles)
    });
    let second = flattened(synthetic_tree(100, 10));
    crate::store::sync(&mut store, &prior, &second, 1);
    assert_eq!(crate::store::flattened_node_count(&second), count!());
    assert_eq!(count!(), 2 + 100 + 7);
}

/// Scale check (NWS-2942): 100k nodes and a 2k-set × 20k-component
/// library, new implementation timed against the reference one. Ignored by
/// default (it builds a large store); run with
/// `cargo test --lib nws_2942_scale -- --ignored --nocapture`.
#[test]
#[ignore]
fn nws_2942_scale() {
    use std::time::Instant;
    let mut recs = synthetic_tree(100_000, 200);
    recs.extend(synthetic_library(2_000, 10).into_iter().skip(2));
    let dir = tempfile::tempdir().unwrap();
    let mut store = crate::open_store!(&dir.path().join("db"));
    let flat = flattened(recs);
    crate::store::sync(&mut store, &Default::default(), &flat, 0);
    let time = |label: &str, f: &mut dyn FnMut() -> String| {
        let start = Instant::now();
        let out = f();
        eprintln!(
            "{label:<28} {:>8.1} ms",
            start.elapsed().as_secs_f64() * 1e3
        );
        out
    };
    store.rtx(
        |(
            (nodes, _, _, _, _, _, by_type),
            components_t,
            sets,
            styles,
            variables,
            collections,
            meta,
            _,
            _,
            _,
            _,
            snapshot,
        )| {
            let n = nodes.iter().count();
            eprintln!("nodes={n} components={}", components_t.iter().count());
            time("status (count)  before", &mut || {
                status(&nodes, &meta, &variables, &collections, &snapshot, None)
                    .unwrap()
                    .to_string()
            });
            time("status (count)  after", &mut || {
                status(&nodes, &meta, &variables, &collections, &snapshot, Some(n))
                    .unwrap()
                    .to_string()
            });
            let before = time("stats max_depth before", &mut || {
                reference::max_depth(&nodes).to_string()
            });
            let after = time("stats           after", &mut || {
                stats(&nodes, &components_t, &sets, &styles, &variables, &by_type).unwrap()
                    ["max_depth"]
                    .to_string()
            });
            assert_eq!(before, after);
            time("vars infer      before", &mut || {
                let all: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();
                reference::infer(&all).to_string()
            });
            time("vars            after", &mut || {
                vars(&nodes, &variables, &collections, &snapshot, None, false)
                    .unwrap()
                    .to_string()
            });
            let before = time("components      before", &mut || {
                reference::components(&nodes, &components_t, &sets).to_string()
            });
            let after = time("components      after", &mut || {
                components(&nodes, &components_t, &sets)
                    .unwrap()
                    .to_string()
            });
            assert_eq!(before, after);
        },
    );
}
