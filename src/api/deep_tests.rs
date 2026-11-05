use super::*;
use crate::api::test_http::{Peer, Reply};
use crate::mcp::ToolOutput;
use crate::sessions::open_session_with_api;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

#[path = "../../tests/fixtures/deep_file.rs"]
mod deep_file;

#[test]
fn pulls_500_nested_frames_over_rest() {
    // A cold database open can exceed the ordinary peer's 15s startup
    // allowance on a busy test machine, before any HTTP request is sent.
    let budget = Duration::from_secs(60);
    let peer = Peer::with_startup_timeout(
        vec![
            Reply::Json(deep_file::file(500)),
            Reply::Complete(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
        ],
        budget,
    );
    let api = Arc::new(UreqApi::with_base_url(
        "synthetic-token".into(),
        peer.url.clone(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let mut session = open_session_with_api(
        dir.path().join("db"),
        "ABC123".into(),
        Some("ABC123"),
        Some(api),
    )
    .unwrap();
    session.request_pull(false, Duration::from_secs(300));
    let deadline = Instant::now() + budget;
    loop {
        session.maintain();
        let status = session.status().unwrap();
        match status["pull"]["job"]["state"].as_str() {
            Some("succeeded") => {
                assert_eq!(status["nodes"], 503);
                break;
            }
            Some("failed") => panic!("pull failed: {status}"),
            _ => assert!(Instant::now() < deadline, "pull did not complete: {status}"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let ToolOutput::Json(leaf) = session
        .dispatch("figmog_node", &json!({"id":"2:1"}))
        .unwrap()
    else {
        panic!("expected JSON");
    };
    assert_eq!(leaf["characters"], "Deep leaf");
}
