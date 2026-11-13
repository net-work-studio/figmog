#![recursion_limit = "256"]

//! NWS-2941: post-write compaction. The benchmark builds a synthetic
//! 100,001-node file (never derived from a real one), pulls it twice —
//! once with `FIGMOG_NO_COMPACT=1`, once compacted — and times cold CLI
//! reads against each.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A synthetic `GET /v1/files/:key` response of exactly `nodes` nodes
/// (at least 3): the document, one page (`0:1`), and frames of up to 99
/// text/rectangle/instance leaves, some bound to variables. Deterministic.
fn synthetic_file(nodes: usize) -> Value {
    const PER_FRAME: usize = 99;
    let mut remaining = nodes - 3; // document, page, component
    let mut frames = Vec::new();
    let mut frame = 0;
    while remaining > 0 {
        remaining -= 1;
        let leaves = remaining.min(PER_FRAME);
        remaining -= leaves;
        let children: Vec<Value> = (0..leaves)
            .map(|i| {
                let id = format!("{}:{}", frame + 10, i + 1);
                match i % 3 {
                    0 => json!({"id": id, "name": format!("Label {frame}-{i}"), "type": "TEXT",
                        "characters": format!("Synthetic copy number {frame} {i} lorem ipsum dolor"),
                        "style": {"fontFamily": "Basis", "fontSize": 14.0},
                        "styles": {"text": "S:2"}, "children": []}),
                    1 => json!({"id": id, "name": "Button", "type": "INSTANCE",
                        "componentId": "2:2", "children": []}),
                    _ => json!({"id": id, "name": format!("Box {i}"), "type": "RECTANGLE",
                        "absoluteBoundingBox": {"x": i as f64, "y": 0.0, "width": 10.0, "height": 10.0},
                        "fills": [{"type": "SOLID", "color": {"r": 0.1, "g": 0.2, "b": 0.3, "a": 1.0},
                            "boundVariables": {"color": {"type": "VARIABLE_ALIAS",
                                "id": format!("VariableID:{}", i % 7)}}}],
                        "styles": {"fill": "S:1"}, "children": []}),
                }
            })
            .collect();
        frames.push(
            json!({"id": format!("{}:0", frame + 10), "name": format!("Frame {frame}"),
            "type": "FRAME", "children": children}),
        );
        frame += 1;
    }
    json!({"name": "Synthetic", "version": "1", "lastModified": "2026-01-01T12:00:00Z",
    "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT", "children": [
        {"id": "0:1", "name": "Page", "type": "CANVAS", "children": frames},
        {"id": "2:2", "name": "Button", "type": "COMPONENT", "children": []}
    ]},
    "components": {"2:2": {"key": "k22", "name": "Button", "description": "", "remote": false}},
    "styles": {
        "S:1": {"key": "sk1", "name": "Brand/Primary", "styleType": "FILL", "description": "", "remote": false},
        "S:2": {"key": "sk2", "name": "Body", "styleType": "TEXT", "description": "", "remote": false}
    }})
}

fn journal_bytes(db: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(db)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jnl"))
        .map(|entry| {
            let metadata = entry.metadata().unwrap();
            metadata.len().min(metadata.blocks() * 512)
        })
        .sum()
}

fn run(db: &Path, args: &[&str], no_compact: bool) -> (Vec<u8>, Duration) {
    let mut command = common::command();
    command.args(args).args(["--db", db.to_str().unwrap()]);
    if no_compact {
        command.env("FIGMOG_NO_COMPACT", "1");
    } else {
        command.env_remove("FIGMOG_NO_COMPACT");
    }
    let start = Instant::now();
    let output = command.output().unwrap();
    let elapsed = start.elapsed();
    assert!(output.status.success(), "{args:?}: {output:?}");
    (output.stdout, elapsed)
}

/// Best of `n` runs: process startup noise only ever adds time.
fn best(db: &Path, args: &[&str], n: usize) -> (Vec<u8>, Duration) {
    let mut out = run(db, args, true);
    for _ in 1..n {
        let next = run(db, args, true);
        assert_eq!(next.0, out.0);
        out.1 = out.1.min(next.1);
    }
    out
}

/// `cargo test --test compact -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: pulls a 100,001-node synthetic file twice"]
fn compaction_benchmark_100k_nodes_cold_cli_reads() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.json");
    std::fs::write(&file, serde_json::to_vec(&synthetic_file(100_001)).unwrap()).unwrap();
    let file = file.to_str().unwrap();
    let plain = dir.path().join("plain");
    let compacted = dir.path().join("compacted");

    let (_, plain_pull) = run(&plain, &["pull", "--from-file", file], true);
    let (_, compacted_pull) = run(&compacted, &["pull", "--from-file", file], false);
    // Reads never compact, but keep the baseline honest anyway.
    let plain_journal = journal_bytes(&plain);
    let compacted_journal = journal_bytes(&compacted);
    println!(
        "pull: {:.2}s plain, {:.2}s with compaction; journal {plain_journal} B -> {compacted_journal} B",
        plain_pull.as_secs_f64(),
        compacted_pull.as_secs_f64()
    );
    assert!(compacted_journal < 1_000_000, "{compacted_journal}");

    let reads: [&[&str]; 5] = [
        &["path", "0:1"],
        &["get", "500:3"],
        &["status"],
        &["search", "copy number 7"],
        &["vars"],
    ];
    println!("| command | journal replayed (s) | compacted (s) |");
    println!("|---|---|---|");
    for args in reads {
        let (before, slow) = best(&plain, args, 3);
        let (after, fast) = best(&compacted, args, 3);
        let shown = |out: &[u8]| {
            let v: Value = serde_json::from_slice(out).unwrap();
            // `status` reports when each store was synced.
            if let Some(map) = v.as_object() {
                let mut map = map.clone();
                map.remove("synced_at_unix_ms");
                return Value::Object(map);
            }
            v
        };
        assert_eq!(shown(&before), shown(&after), "{args:?}");
        if args[0] != "status" {
            assert_eq!(before, after, "{args:?} output differs");
        }
        println!(
            "| `{}` | {:.3} | {:.3} |",
            args.join(" "),
            slow.as_secs_f64(),
            fast.as_secs_f64()
        );
        if args == ["path", "0:1"] {
            assert!(fast < Duration::from_millis(100), "cold open took {fast:?}");
        }
    }

    // An unchanged re-pull is zero churn, and leaves the store compact.
    let (churn, _) = run(&compacted, &["pull", "--from-file", file], false);
    let churn: Value = serde_json::from_slice(&churn).unwrap();
    assert_eq!(
        (&churn["added"], &churn["changed"], &churn["removed"]),
        (&json!(0), &json!(0), &json!(0))
    );
    assert!(journal_bytes(&compacted) < 1_000_000);
}

/// Always on: compaction never changes what a command prints. A small
/// mirror's journal is under fjall's sealing threshold, so this covers
/// the skip path end to end; the unit tests in `src/compact.rs` and the
/// benchmark above cover a real compaction.
#[test]
fn small_pulls_print_the_same_with_and_without_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.json");
    std::fs::write(&file, serde_json::to_vec(&synthetic_file(2_000)).unwrap()).unwrap();
    let file = file.to_str().unwrap();
    let mut outputs = Vec::new();
    for (name, no_compact) in [("plain", true), ("compacted", false)] {
        let db = dir.path().join(name);
        let (churn, _) = run(&db, &["pull", "--from-file", file], no_compact);
        let reads: Vec<Vec<u8>> = [
            &["path", "10:3"][..],
            &["search", "copy number 7"],
            &["vars"],
        ]
        .iter()
        .map(|args| run(&db, args, no_compact).0)
        .collect();
        outputs.push((churn, reads));
    }
    assert_eq!(outputs[0], outputs[1]);
}
