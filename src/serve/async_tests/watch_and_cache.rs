//! Main's cache-scan and queued-watch invariants exercised with async workers.
use super::*;
use crate::model::{FileMeta, Id, ImageBlobRec, Rec};
use std::collections::BTreeMap;

fn seed_cache(path: &std::path::Path, prior_version: Option<&str>) {
    let mut st = crate::open_store!(path);
    if let Some(version) = prior_version {
        st.wtx(|tx| {
            tx.upsert(
                &Id::Meta,
                &Rec::Meta(FileMeta {
                    name: "Synthetic".into(),
                    version: version.into(),
                    last_modified: "t".into(),
                    synced_at_unix_ms: 0,
                }),
            );
        });
    }
    for version in ["0", "1"] {
        crate::cache::store(&mut st, "get_code", version, version, &json!([])).unwrap();
        for kind in ["render", "fill"] {
            let subject = format!("synthetic-{kind}-{version}");
            let key = crate::images::image_key(kind, &subject, "png", 1000);
            let rec = ImageBlobRec {
                key_hash: key.clone(),
                kind: kind.into(),
                subject,
                format: "png".into(),
                scale_milli: 1000,
                file_version: version.into(),
                bytes: vec![42; 1024 * 1024],
            };
            st.wtx(|tx| {
                tx.upsert(&Id::ImageBlob(key.clone()), &Rec::ImageBlob(rec));
            });
        }
    }
}

fn cache_rows(path: &std::path::Path) -> Vec<Rec> {
    let st = crate::open_store!(path);
    st.rtx(|(_, _, _, _, _, _, _, cache, _, images, _, _)| {
        cache
            .iter()
            .map(|(_, rec)| Rec::ProxyCache(rec))
            .chain(images.iter().map(|(_, rec)| Rec::ImageBlob(rec)))
            .collect()
    })
}

/// Observe the actual scan helpers on the worker thread. Their counters
/// are thread-local so independent tests cannot contaminate each other.
fn cached_harness(prior: Option<&str>) -> (Harness, Vec<Rec>, mpsc::Receiver<(usize, usize)>) {
    let mut h = Harness::new();
    h.manager.sessions.clear();
    let path = h._dir.path().join("ABC1234567890/db");
    seed_cache(&path, prior);
    let expected = cache_rows(&path);
    let (observed, scans) = mpsc::channel();
    let session = sessions::open_session_with_hooks(
        path,
        "ABC1234567890".into(),
        Some("ABC1234567890"),
        Some(h.api.clone()),
        sessions::WorkerHooks {
            on_phase: Some(Arc::new(move |phase| {
                if phase == "publishing" {
                    observed.send(store::STALE_SCAN_COUNTS.get()).unwrap();
                }
            })),
        },
    )
    .unwrap();
    h.manager.sessions.push(session);
    (h, expected, scans)
}

#[test]
fn unchanged_async_pulls_skip_both_cache_scans_with_large_images() {
    let (mut h, expected, scans) = cached_harness(Some("1"));
    for _ in 0..3 {
        *h.api.response.lock().unwrap() = Ok(fixture("1"));
        h.value("figmog_sync", json!({}));
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fetch"
        );
        h.release.send(()).unwrap();
        h.wait_for("succeeded");
        assert_eq!(scans.recv_timeout(Duration::from_secs(5)).unwrap(), (0, 0));
    }
    h.manager.sessions.clear();
    assert_eq!(
        cache_rows(&h._dir.path().join("ABC1234567890/db")),
        expected
    );
}

#[test]
fn initial_and_changed_async_pulls_scan_and_evict_only_mismatched_cache_rows() {
    for prior in [None, Some("0")] {
        let (mut h, rows, scans) = cached_harness(prior);
        let expected: Vec<_> = rows
            .into_iter()
            .filter(|row| match row {
                Rec::ProxyCache(rec) => rec.file_version == "1",
                Rec::ImageBlob(rec) => rec.file_version == "1",
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(expected.len(), 3);
        h.value("figmog_sync", json!({}));
        assert_eq!(
            h.entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fetch"
        );
        h.release.send(()).unwrap();
        h.wait_for("succeeded");
        assert_eq!(scans.recv_timeout(Duration::from_secs(5)).unwrap(), (1, 1));
        h.manager.sessions.clear();
        assert_eq!(
            cache_rows(&h._dir.path().join("ABC1234567890/db")),
            expected
        );
    }
}

fn queue_requests(tx: &mpsc::Sender<Incoming>, tool: &str) {
    for id in 1..=8 {
        tx.send(Incoming::Stdio(
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"}).to_string(),
        ))
        .unwrap();
        tx.send(socket_request(id, tool)).unwrap();
    }
}

fn socket_request(id: u64, tool: &str) -> Incoming {
    Incoming::Socket(
        1,
        json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
        "params":{"name":tool,"arguments":{}}})
        .to_string(),
    )
}

/// Read `count` responses, keyed by JSON-RPC id: heavy reads answer from
/// read workers (NWS-2962), so one stream's responses may arrive in any
/// order.
fn responses(reader: &mut BufReader<UnixStream>, count: u64) -> BTreeMap<u64, Value> {
    (0..count)
        .map(|_| {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["result"]["isError"], false, "{frame}");
            let text = frame["result"]["content"][0]["text"].as_str().unwrap();
            (
                frame["id"].as_u64().unwrap(),
                serde_json::from_str(text).unwrap(),
            )
        })
        .collect()
}

#[test]
fn overdue_watch_is_not_starved_by_prequeued_stdio_and_socket_traffic() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h.api.block_meta.store(true, Ordering::Relaxed);
    let Harness {
        release,
        entered,
        api: _,
        manager,
        _dir: _dir_guard,
        gate_stages: _,
    } = h;
    let (tx, rx) = mpsc::channel();
    let (client, socket) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let registry: ConnRegistry = Arc::default();
    registry.register(1, socket).unwrap();
    queue_requests(&tx, "figmog_status");
    let worker = std::thread::spawn(move || {
        serve_loop_with_output(
            manager,
            None,
            false,
            false,
            Duration::from_secs(3600),
            Instant::now() - Duration::from_secs(1),
            rx,
            registry,
            |_| panic!("notifications must not produce responses"),
        )
    });
    let mut reader = BufReader::new(client);
    let first = responses(&mut reader, 8);
    assert_eq!(
        first.keys().copied().collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
    for status in first.values() {
        assert_eq!(
            status["version"], "1",
            "gated refresh must retain the committed mirror"
        );
        assert_eq!(
            status["pull"]["polling"], true,
            "the overdue poll must be scheduled before queued reads"
        );
    }
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "metadata"
    );
    release.send(()).unwrap();
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(5)).unwrap(),
        "fetch"
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    for id in 9.. {
        tx.send(socket_request(id, "figmog_status")).unwrap();
        let status = responses(&mut reader, 1).remove(&id).unwrap();
        if status["version"] == "2" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queued traffic must not starve publication"
        );
    }
    tx.send(Incoming::StdinClosed).unwrap();
    worker.join().unwrap().unwrap();
}

#[test]
fn no_watch_answers_prequeued_requests_without_polling_an_overdue_deadline() {
    let mut h = Harness::new();
    h.value("figmog_sync", json!({}));
    h.entered.recv_timeout(Duration::from_secs(5)).unwrap();
    h.release.send(()).unwrap();
    h.wait_for("succeeded");
    h.api.block_meta.store(true, Ordering::Relaxed);
    let Harness {
        release: _release_guard,
        entered,
        api: _,
        manager,
        _dir: _dir_guard,
        gate_stages: _,
    } = h;
    let (tx, rx) = mpsc::channel();
    let (client, socket) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let registry: ConnRegistry = Arc::default();
    registry.register(1, socket).unwrap();
    queue_requests(&tx, "figmog_files");
    tx.send(Incoming::StdinClosed).unwrap();
    serve_loop_with_output(
        manager,
        None,
        true,
        false,
        Duration::from_secs(10),
        Instant::now() - Duration::from_secs(1),
        rx,
        registry,
        |_| panic!("unexpected response"),
    )
    .unwrap();
    let mut reader = BufReader::new(client);
    let files = responses(&mut reader, 8);
    assert_eq!(
        files.keys().copied().collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
    for listed in files.values() {
        assert_eq!(listed[0]["version"], "1");
    }
    assert!(matches!(
        entered.try_recv(),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn round_robin_watch_preserves_per_file_retry_after_without_blocking_other_files() {
    let mut first = Harness::new();
    first.mirror_from_watch();
    let mut second = Harness::new();
    second.mirror_from_watch();
    let mut other = second.manager.sessions.pop().unwrap();
    other.key = "OTHER123456789".into();
    first.manager.sessions.push(other);
    first.api.block_meta.store(true, Ordering::Relaxed);
    second.api.block_meta.store(true, Ordering::Relaxed);
    let interval = Duration::from_secs(10);
    let base = Instant::now() + Duration::from_secs(60);
    let mut cursor = 0;
    for (round, index, wait) in [(0, 0, 0), (1, 1, 0), (2, 0, 3600), (3, 1, 0)] {
        let api = if index == 0 { &first.api } else { &second.api };
        let entered = if index == 0 {
            &first.entered
        } else {
            &second.entered
        };
        let release = if index == 0 {
            &first.release
        } else {
            &second.release
        };
        *api.metadata_error.lock().unwrap() = Some(ApiError::RateLimited {
            retry_after: Duration::from_secs(wait),
        });
        let now = base + Duration::from_secs(round * 5);
        watch_tick_at(&mut first.manager, &mut cursor, interval, now);
        assert_eq!(
            first.manager.sessions[index].status().unwrap()["pull"]["polling"],
            true
        );
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "metadata"
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while first.manager.sessions[index].status().unwrap()["pull"]["polling"] == true {
            first.manager.sessions[index].maintain_at(now, true);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    watch_tick_at(
        &mut first.manager,
        &mut cursor,
        interval,
        base + Duration::from_secs(20),
    );
    assert_eq!(
        first.manager.sessions[0].status().unwrap()["pull"]["polling"],
        false,
        "the first file's Retry-After must still apply"
    );
    assert!(matches!(
        first.entered.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let files = first.value("figmog_files", json!({}));
    assert_eq!(files[0]["version"], "1");
    assert_eq!(files[1]["version"], "1");
}
