//! Synthetic JSON built as text so the fixture itself never needs a deep parser.
pub fn file(frames: usize) -> String {
    let mut text = String::from(
        r#"{"name":"Deep","version":"1","lastModified":"2026-11-04T00:00:00Z","document":{"id":"0:0","type":"DOCUMENT","children":[{"id":"0:1","type":"CANVAS","children":["#,
    );
    for i in 1..=frames {
        text.push_str(&format!(
            r#"{{"id":"1:{i}","name":"Frame {i}","type":"FRAME","children":["#
        ));
    }
    text.push_str(r#"{"id":"2:1","type":"TEXT","name":"Leaf","characters":"Deep leaf"}"#);
    for _ in 0..frames {
        text.push_str("]}");
    }
    text.push_str("]}]}}");
    text
}
