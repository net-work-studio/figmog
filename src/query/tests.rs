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
