#![recursion_limit = "256"]

//! `figmog assets export` (NWS-2952) end to end on a synthetic project,
//! offline: `fixtures/assets/icons.json` (an `Icon` component set with
//! four variants — a one-color stroked arrow, a one-color star with a clip
//! path, a two-color gradient logo with a mask, and a hostile render — and
//! a `Hero` frame with an image fill and an arrow instance) is pulled into
//! the project's mirror, and the render cache is seeded with the SVGs in
//! `fixtures/assets/renders/` (pinned to the mirrored version, NWS-2960)
//! and the fill with `fixtures/assets/photo.png`, so no Figma call is made.
//!
//! Done when: the synthetic icon set exports to React components, and
//! re-exporting an unchanged file changes no files.
//!
//! The exported tree is golden (`fixtures/assets/export/`, with `[assets]
//! components = "jsx"` and `current_color = true`); `examples/slice/app`'s
//! `icons.test.mjs` renders those components with React to check that
//! repeated instances never share an id. Regenerate after an intended
//! change: `FIGMOG_BLESS=1 cargo test --test assets`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const KEY: &str = "figmogassetsfixture12";
const VERSION: &str = "12";
const IMAGE_REF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
const ICONS: [&str; 4] = ["1:2", "1:3", "1:4", "1:5"];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/assets")
}

fn figmog(root: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    common::command()
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--no-socket")
        .args(args)
        .assert()
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

/// A project with `assets` as its `[assets]` table, the fixture pulled,
/// and the cache seeded with the renders of `icons` and the image fill.
fn project(assets: &str, icons: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n\n[assets]\n{assets}"),
    )
    .unwrap();
    let response = fixtures().join("icons.json");
    figmog(root, &["pull", "--from-file", response.to_str().unwrap()]).success();
    seed(&root.join(".figmog").join(KEY).join("db"), icons);
    dir
}

fn seed(db: &Path, icons: &[&str]) {
    use figmog::model::{Id, ImageBlobRec, ImagePinRec, Rec};
    let mut st = figmog::open_store!(db);
    st.wtx(|tx| {
        for id in icons {
            let key = figmog::images::image_key("render", id, "svg", 1000);
            let svg =
                fs::read(fixtures().join(format!("renders/{}.svg", id.replace(':', "-")))).unwrap();
            tx.upsert(
                &Id::ImageBlob(key.clone()),
                &Rec::ImageBlob(ImageBlobRec {
                    key_hash: key.clone(),
                    kind: "render".into(),
                    subject: id.to_string(),
                    format: "svg".into(),
                    scale_milli: 1000,
                    file_version: VERSION.into(),
                    bytes: svg,
                }),
            );
            tx.upsert(
                &Id::ImagePin(key.clone()),
                &Rec::ImagePin(ImagePinRec {
                    generation: figmog::images::RENDER_PIN_GENERATION,
                    file_version: VERSION.into(),
                }),
            );
        }
        let key = figmog::images::image_key("fill", IMAGE_REF, "", 1000);
        tx.upsert(
            &Id::ImageBlob(key.clone()),
            &Rec::ImageBlob(ImageBlobRec {
                key_hash: key,
                kind: "fill".into(),
                subject: IMAGE_REF.into(),
                format: "png".into(),
                scale_milli: 1000,
                file_version: VERSION.into(),
                bytes: fs::read(fixtures().join("photo.png")).unwrap(),
            }),
        );
    });
}

const GOLDEN_ASSETS: &str = "icons = \"src/icons\"\nimages = \"public/figma\"\ncomponents = \"jsx\"\ncurrent_color = true\n";

/// Every file under `root` except figmog's own, by relative path.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            if rel == ".figmog" || rel == "figmog.toml" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn mtimes(root: &Path) -> BTreeMap<String, std::time::SystemTime> {
    tree(root)
        .keys()
        .map(|rel| {
            let m = fs::metadata(root.join(rel)).unwrap().modified().unwrap();
            (rel.clone(), m)
        })
        .collect()
}

fn assert_golden(files: &BTreeMap<String, Vec<u8>>) {
    let golden = fixtures().join("export");
    if std::env::var_os("FIGMOG_BLESS").is_some() {
        let _ = fs::remove_dir_all(&golden);
        for (rel, bytes) in files {
            let path = golden.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }
    let expected = tree(&golden);
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "tests/fixtures/assets/export is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
    );
    for (rel, bytes) in files {
        assert_eq!(
            String::from_utf8_lossy(bytes),
            String::from_utf8_lossy(&expected[rel]),
            "tests/fixtures/assets/export/{rel} is stale — rerun with FIGMOG_BLESS=1 if the change is intended"
        );
    }
}

#[test]
fn synthetic_icon_set_exports_to_react_components_and_reexport_changes_nothing() {
    let dir = project(GOLDEN_ASSETS, &ICONS);
    let root = dir.path();
    let out = figmog(
        root,
        &["assets", "export", "--icons", "1:1", "--images", "2:1"],
    )
    .success();
    let summary = json_of(&out.get_output().stdout);
    let components: Vec<&str> = summary["icons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["component"].as_str().unwrap())
        .collect();
    assert_eq!(
        components,
        ["IconArrowRight", "IconLogo", "IconStar", "IconUnsafe"]
    );
    assert_eq!(summary["skipped"], json!([]));
    assert_eq!(summary["unchanged"], 0);
    assert_eq!(summary["written"], 11);

    // Single-color icons follow the text color; the logo keeps its colors.
    let colors: Vec<(&str, &str, bool)> = summary["icons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["component"].as_str().unwrap(),
                i["colors"].as_str().unwrap(),
                i["current_color"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        colors,
        [
            ("IconArrowRight", "single", true),
            ("IconLogo", "multi", false),
            ("IconStar", "single", true),
            ("IconUnsafe", "single", true),
        ]
    );
    let image = &summary["images"][0];
    let sha = "a3979dcc5fa7ded81490fbc06a6479e2356b3018da7c1cdff6212017f1404cc5";
    assert_eq!(image["sha256"], sha);
    assert_eq!(image["path"], format!("public/figma/{}.png", &sha[..16]));
    assert_eq!(image["url"], format!("/figma/{}.png", &sha[..16]));
    assert_eq!(image["nodes"], json!(["2:2"]));

    let first = tree(root);
    assert_golden(&first);
    let arrow = String::from_utf8(first["src/icons/IconArrowRight.jsx"].clone()).unwrap();
    assert!(
        arrow.contains("export function IconArrowRight(props) {"),
        "{arrow}"
    );
    assert!(arrow.contains("stroke=\"currentColor\""), "{arrow}");

    // Re-export: byte-identical, nothing rewritten (mtimes kept), by the
    // same flags and by the roots the manifest recorded.
    let before = mtimes(root);
    std::thread::sleep(std::time::Duration::from_millis(20));
    for args in [
        &["assets", "export", "--icons", "1:1", "--images", "2:1"][..],
        &["assets", "export"][..],
    ] {
        let again = json_of(&figmog(root, args).success().get_output().stdout);
        assert_eq!(again["written"], 0, "{again}");
        assert_eq!(again["unchanged"], 11);
        assert_eq!(tree(root), first);
        assert_eq!(mtimes(root), before);
    }
}

#[test]
fn spec_references_the_manifest_instead_of_raw_ids() {
    let dir = project(GOLDEN_ASSETS, &ICONS);
    let root = dir.path();
    let spec_of = |root: &Path| {
        json_of(
            &figmog(root, &["spec", "2:1", "--no-tailwind"])
                .success()
                .get_output()
                .stdout,
        )
    };
    let child = |spec: &Value, id: &str| {
        spec["node"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
            .clone()
    };

    // No manifest: raw imageRef, no icon reference (the pre-NWS-2952 shape).
    let before = spec_of(root);
    assert_eq!(
        child(&before, "2:2")["content"]["assets"],
        json!([{"kind": "image", "node_id": "2:2", "paint": "fills[0]",
                "image_ref": IMAGE_REF, "scale_mode": "FILL"}])
    );
    assert!(child(&before, "2:3").get("content").is_none());

    figmog(
        root,
        &["assets", "export", "--icons", "1:1", "--images", "2:1"],
    )
    .success();
    let after = spec_of(root);
    let sha16 = "a3979dcc5fa7ded8";
    assert_eq!(
        child(&after, "2:2")["content"]["assets"],
        json!([{"kind": "image", "node_id": "2:2", "paint": "fills[0]",
                "image_ref": IMAGE_REF, "scale_mode": "FILL",
                "path": format!("public/figma/{sha16}.png"),
                "url": format!("/figma/{sha16}.png")}])
    );
    assert_eq!(
        child(&after, "2:3")["content"]["assets"],
        json!([{"kind": "icon", "node_id": "2:3", "component": "IconArrowRight",
                "path": "src/icons/IconArrowRight.jsx",
                "svg": "src/icons/svg/IconArrowRight.svg",
                "jsx": "<IconArrowRight />"}])
    );
    let reasons: Vec<&str> = after["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| u["id"] == "2:2")
        .map(|u| u["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        [format!(
            "fills[0]: IMAGE paint: exported as public/figma/{sha16}.png (content.assets); its scaling and cropping are not translated to CSS"
        )]
    );

    // A manifest figmog can't trust fails the spec, naming the file.
    fs::write(
        root.join("figmog.assets.json"),
        r#"{"icons": [{"node_id": "1:2", "component": "x</script>", "path": "a", "svg": "b"}]}"#,
    )
    .unwrap();
    let out = figmog(root, &["spec", "2:1"]).failure().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("figmog.assets.json: icons[0].component: not a component name"),
        "{stderr}"
    );
}

#[test]
fn hostile_svg_markup_is_sanitized() {
    let dir = project(GOLDEN_ASSETS, &ICONS);
    let root = dir.path();
    let out = figmog(root, &["assets", "export", "--icons", "1:5"]).success();
    let summary = json_of(&out.get_output().stdout);
    let removed: Vec<&str> = summary["icons"][0]["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    for expected in [
        "dropped <script>",
        "dropped <style>",
        "dropped <foreignObject>",
        "dropped <a>",
        "dropped attribute onload on <svg>",
        "dropped attribute onclick on <path>",
        "dropped unsafe filter value on <path>",
        "dropped unsafe href value on <use>",
        "dropped unsafe xlink:href value on <image>",
    ] {
        assert!(removed.contains(&expected), "{expected} not in {removed:?}");
    }
    for file in ["src/icons/IconUnsafe.jsx", "src/icons/svg/IconUnsafe.svg"] {
        let text = fs::read_to_string(root.join(file))
            .unwrap()
            .to_ascii_lowercase();
        for needle in [
            "script",
            "onload",
            "onclick",
            "@import",
            "foreignobject",
            "iframe",
            "attacker",
            "javascript",
            "<a",
            "<image",
            "<use",
        ] {
            assert!(
                !text.contains(needle),
                "{needle} survived in {file}: {text}"
            );
        }
        assert!(text.contains("m4 4h20v20h4z"), "{text}");
    }
}

#[test]
fn export_refuses_foreign_files_and_symlinks() {
    let dir = project(GOLDEN_ASSETS, &ICONS);
    let root = dir.path();
    let args = ["assets", "export", "--icons", "1:1", "--images", "2:1"];
    fs::create_dir_all(root.join("src/icons")).unwrap();
    let hand_written = "export function IconStar() { return null }\n";
    fs::write(root.join("src/icons/IconStar.jsx"), hand_written).unwrap();
    let out = figmog(root, &args).failure().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("src/icons/IconStar.jsx exists and was not generated by figmog"),
        "{stderr}"
    );
    // Nothing at all was written.
    assert_eq!(
        tree(root).keys().collect::<Vec<_>>(),
        ["src/icons/IconStar.jsx"]
    );
    // An image file that isn't the content it is named after: refused too.
    fs::remove_file(root.join("src/icons/IconStar.jsx")).unwrap();
    figmog(root, &args).success();
    let photo = root.join("public/figma/a3979dcc5fa7ded8.png");
    fs::write(&photo, b"not the photo").unwrap();
    let out = figmog(root, &args).failure().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("exists with other content"), "{stderr}");
    // --force overwrites.
    fs::write(root.join("src/icons/IconStar.jsx"), hand_written).unwrap();
    let forced = json_of(
        &figmog(root, &["assets", "export", "--force"])
            .success()
            .get_output()
            .stdout,
    );
    assert_eq!(forced["written"], 2, "{forced}");
    assert_eq!(
        fs::read(&photo).unwrap(),
        fs::read(fixtures().join("photo.png")).unwrap()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        fs::remove_dir_all(root.join("src/icons")).unwrap();
        symlink(outside.path(), root.join("src/icons")).unwrap();
        for force in [false, true] {
            let mut args = vec!["assets", "export"];
            if force {
                args.push("--force");
            }
            let out = figmog(root, &args).failure().code(1);
            let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
            assert!(stderr.contains("src/icons is a symlink"), "{stderr}");
        }
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[test]
fn a_skipped_asset_writes_nothing_unless_partial_and_tsx_is_the_default() {
    // The logo's render is not cached and there is no token.
    let dir = project("", &["1:2", "1:3", "1:5"]);
    let root = dir.path();
    let args = ["assets", "export", "--icons", "1:1"];
    let out = figmog(root, &args).failure().code(1);
    let summary = json_of(&out.get_output().stdout);
    assert_eq!(summary["written"], 0);
    assert_eq!(summary["skipped"][0]["node_id"], "1:4");
    assert!(
        summary["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("FIGMA_TOKEN"),
        "{summary}"
    );
    assert!(tree(root).is_empty(), "{:?}", tree(root).keys());

    figmog(root, &["assets", "export", "--icons", "1:1", "--partial"])
        .failure()
        .code(1);
    let files: Vec<String> = tree(root).keys().cloned().collect();
    assert_eq!(
        files,
        [
            "figmog.assets.json",
            "src/icons/IconArrowRight.tsx",
            "src/icons/IconStar.tsx",
            "src/icons/IconUnsafe.tsx",
            "src/icons/index.ts",
            "src/icons/svg/IconArrowRight.svg",
            "src/icons/svg/IconStar.svg",
            "src/icons/svg/IconUnsafe.svg",
        ]
    );
    let star = fs::read_to_string(root.join("src/icons/IconStar.tsx")).unwrap();
    assert!(
        star.contains("import type { SVGProps } from \"react\";"),
        "{star}"
    );
    assert!(
        star.contains("export function IconStar(props: SVGProps<SVGSVGElement>) {"),
        "{star}"
    );
    // currentColor is opt-in: off by default.
    assert!(star.contains("fill=\"#0F172A\""), "{star}");
    let manifest = json_of(&fs::read(root.join("figmog.assets.json")).unwrap());
    assert_eq!(manifest["icons"].as_array().unwrap().len(), 3);
}

#[test]
fn assets_needs_a_project_and_roots() {
    let (_dir, db) = common::fixture_db();
    let out = common::command()
        .args([
            "--db",
            db.to_str().unwrap(),
            "assets",
            "export",
            "--icons",
            "3:1",
        ])
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("does not take --db"), "{stderr}");

    let dir = project("", &[]);
    let out = figmog(dir.path(), &["assets", "export"]).failure().code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("nothing to export"), "{stderr}");
    // A bad [assets] value is a figmog.toml error, naming the line.
    fs::write(
        dir.path().join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n[assets]\nicons = \"../outside\"\n"),
    )
    .unwrap();
    let out = figmog(dir.path(), &["assets", "export", "--icons", "1:1"])
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("figmog.toml:4: assets.icons"), "{stderr}");
    // The manifest can't live inside a folder figmog writes itself.
    fs::write(
        dir.path().join("figmog.toml"),
        format!("[files]\ndefault = \"{KEY}\"\n[assets]\nmanifest = \"src/icons/index.ts\"\n"),
    )
    .unwrap();
    let out = figmog(dir.path(), &["assets", "export", "--icons", "1:1"])
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("must not be inside the icons folder"),
        "{stderr}"
    );
}
