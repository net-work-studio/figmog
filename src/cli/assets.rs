//! `figmog assets export` (NWS-2952): icons as sanitized SVG plus React
//! components, image fills as content-named files, and the manifest
//! `figmog spec` reads — all inside the project, as `[assets]` in
//! `figmog.toml` says ([`crate::project::AssetSettings`]).
//!
//! **Reads.** The icon and image roots' subtrees (`figmog_subtree`) and
//! the component inventory (`figmog_components`), then the icons' SVG
//! renders and the image fills — through a running serve's socket when
//! one answers (`figmog_images`, fills with `renders: false`), else from
//! the store opened directly, exactly like `figmog images`: cache hits
//! (pinned to the mirrored version, NWS-2960) cost nothing; misses spend
//! Figma API budget and need a token. Everything between the reads and
//! the files is pure ([`crate::assets::plan`]).
//!
//! **Writes.** Every target is checked before anything is written: it must
//! be inside the project with no symlink anywhere on its path (the file or
//! any directory below the project root), and an existing file is replaced
//! only when figmog generated it — a text file starting with figmog's
//! generated header, an image whose bytes are its content-hash name — or
//! with `--force`. One refusal and nothing is written. A file whose bytes
//! are already right is left alone (no mtime churn), so re-exporting an
//! unchanged file changes no files. Files a previous export wrote that
//! this one no longer produces are reported as `stale`, never deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::{Db, socket, write_json};
use crate::api::{FigmaApi, UreqApi};
use crate::assets::{self, AssetIndex, IconInput, ImageInput, Inventory, Kind, Plan, Sources};
use crate::dispatch::ReadCommand;
use crate::images;
use crate::project::{Layout, check_project_path};

#[derive(Subcommand)]
pub(super) enum AssetsCommand {
    /// Export icons (sanitized SVG + React components) and image fills
    /// (named by content) into figmog.toml's `[assets]` folders, and write
    /// the asset manifest. Without --icons/--images, repeats the roots the
    /// manifest records.
    Export {
        /// An icon root: a component set (each variant is an icon), a
        /// page, section or frame of components, or one node. Repeatable.
        #[arg(long = "icons", value_name = "NODE")]
        icons: Vec<String>,
        /// An image root: every visible image fill under it is exported.
        /// Repeatable.
        #[arg(long = "images", value_name = "NODE")]
        images: Vec<String>,
        /// Overwrite files figmog did not generate. Never writes through a
        /// symlink or outside the project.
        #[arg(long)]
        force: bool,
        /// Write what could be exported even when some icons or images
        /// were skipped (the manifest then lists only those). Default: a
        /// skipped asset writes nothing.
        #[arg(long)]
        partial: bool,
    },
}

/// What the reads returned.
struct Fetched {
    icon_trees: Vec<Value>,
    image_trees: Vec<Value>,
    inventory: Value,
}

/// Rendered or fetched bytes by node id / imageRef, or why not.
type Blobs = BTreeMap<String, Result<Vec<u8>, String>>;

pub(super) fn run(
    db: &Db,
    socket_root: Option<&socket::Route>,
    command: &AssetsCommand,
) -> Result<(), String> {
    let AssetsCommand::Export {
        icons,
        images,
        force,
        partial,
    } = command;
    let layout = db
        .layout
        .as_ref()
        .ok_or("assets export writes into the project and does not take --db")?;
    let settings = layout.settings().assets;
    let root = &layout.project_root;
    for (key, dir) in [("icons", &settings.icons), ("images", &settings.images)] {
        let dir = dir.trim_end_matches('/');
        if settings.manifest == dir || settings.manifest.starts_with(&format!("{dir}/")) {
            return Err(format!(
                "[assets] manifest ({}) must not be inside the {key} folder ({dir}): figmog writes that folder's files itself",
                settings.manifest
            ));
        }
    }

    let previous = previous_manifest(layout, &settings.manifest, *force)?;
    let sources = if icons.is_empty() && images.is_empty() {
        previous
            .as_ref()
            .map(|p| p.sources.clone())
            .unwrap_or_default()
    } else {
        // Sorted, so the manifest doesn't depend on the flags' order.
        let canonical = |ids: &[String]| -> Vec<String> {
            images::normalize_ids(ids)
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        Sources {
            icons: canonical(icons),
            images: canonical(images),
        }
    };
    if sources.icons.is_empty() && sources.images.is_empty() {
        return Err(format!(
            "nothing to export: pass --icons and/or --images node ids (the manifest, {}, records them for the next run)",
            settings.manifest
        ));
    }
    for id in sources.icons.iter().chain(&sources.images) {
        if !assets::is_node_id(id) {
            return Err(format!("not a node id: {id:?}"));
        }
    }

    // Reads, then the planned icons and fills, then their bytes.
    // A running serve answers through its socket (it owns the store);
    // none running: the store is opened directly.
    let routed = socket_root.and_then(|route| Some((route, read_routed(route, db, &sources)?)));
    let (route, fetched) = match routed {
        Some((route, fetched)) => (Some(route), fetched?),
        None => (None, read_direct(db, &sources)?),
    };
    let inventory = Inventory::from_components(&fetched.inventory);
    let mut sources_found = Vec::new();
    for tree in &fetched.icon_trees {
        sources_found.extend(assets::find_icons(tree, &inventory));
    }
    let mut fills: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for tree in &fetched.image_trees {
        assets::find_image_fills(tree, &mut fills);
    }
    let icon_ids: Vec<String> = sources_found
        .iter()
        .map(|s| s.node_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let fill_nodes: Vec<String> = fills
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (renders, blobs) = match route {
        Some(route) => fetch_routed(route, db, &icon_ids, &fill_nodes)?,
        None => fetch_direct(db, &icon_ids, &fill_nodes)?,
    };

    let missing = |what: &str| Err(format!("no {what} was returned"));
    let icon_inputs = sources_found
        .into_iter()
        .map(|source| IconInput {
            svg: renders
                .get(&source.node_id)
                .cloned()
                .unwrap_or_else(|| missing("render")),
            source,
        })
        .collect();
    let image_inputs = fills
        .into_iter()
        .map(|(image_ref, nodes)| ImageInput {
            bytes: blobs
                .get(&image_ref)
                .cloned()
                .unwrap_or_else(|| missing("image")),
            image_ref,
            nodes,
        })
        .collect();
    let plan = assets::plan(&settings, icon_inputs, image_inputs, &sources, &inventory);

    // All or nothing by default: a skipped asset (a fetch that failed, an
    // unusable name or SVG) must not drop its entry from a good manifest.
    let write = plan.skipped.is_empty() || *partial;
    let statuses = if write {
        write_files(root, &plan, *force)?
    } else {
        BTreeMap::new()
    };
    let stale: Vec<&str> = previous
        .iter()
        .flat_map(|p| {
            p.icons
                .iter()
                .flat_map(|i| [i.path.as_str(), i.svg.as_str()])
                .chain(p.images.iter().map(|i| i.path.as_str()))
        })
        .filter(|p| !plan.files.contains_key(*p))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let written = statuses.values().filter(|s| **s == "written").count();
    let files: Vec<Value> = statuses
        .iter()
        .map(|(path, status)| json!({"path": path, "status": status}))
        .collect();
    let summary = json!({
        "manifest": settings.manifest,
        "sources": {"icons": sources.icons, "images": sources.images},
        "icons": plan.icons,
        "images": plan.images,
        "files": files,
        "written": written,
        "unchanged": statuses.len() - written,
        "skipped": plan.skipped,
        "stale": stale,
    });
    write_json(&summary)?;
    match (plan.skipped.len(), write) {
        (0, _) => Ok(()),
        (n, true) => Err(format!(
            "{n} asset(s) skipped — see `skipped` in the output; the others were exported (--partial)"
        )),
        (n, false) => Err(format!(
            "{n} asset(s) could not be exported — see `skipped` in the output; nothing was written (pass --partial to write the others)"
        )),
    }
}

/// The manifest a previous export wrote, if any. One figmog didn't write
/// is refused later by the write check; an unreadable one is an error
/// unless `force` (then it's ignored).
fn previous_manifest(
    layout: &Layout,
    manifest: &str,
    force: bool,
) -> Result<Option<AssetIndex>, String> {
    match AssetIndex::load(&layout.project_root, manifest) {
        Ok(index) => Ok(index),
        Err(_) if force => Ok(None),
        Err(e) => Err(format!(
            "{e} — fix or remove the manifest, or pass --force to replace it"
        )),
    }
}

fn subtree_read(id: &str) -> ReadCommand {
    ReadCommand::Subtree {
        id: id.to_string(),
        depth: None,
        fields: None,
        resolve_vars: false,
    }
}

fn read_direct(db: &Db, sources: &Sources) -> Result<Fetched, String> {
    let st = db.open(|path| crate::open_store!(path))?;
    let fetched = st.rtx(|r| -> Result<Fetched, String> {
        let read = |ids: &[String]| -> Result<Vec<Value>, String> {
            ids.iter().map(|id| subtree_read(id).execute(&r)).collect()
        };
        let icon_trees = read(&sources.icons)?;
        let image_trees = read(&sources.images)?;
        let inventory = ReadCommand::Components.execute(&r)?;
        Ok(Fetched {
            icon_trees,
            image_trees,
            inventory,
        })
    })?;
    drop(st);
    Ok(fetched)
}

/// The reads through a running serve; `None` when none answers.
fn read_routed(
    route: &socket::Route,
    db: &Db,
    sources: &Sources,
) -> Option<Result<Fetched, String>> {
    let call = |read: ReadCommand| -> Option<Result<Value, String>> {
        let (tool, mut args) = read.to_tool_call();
        if let Some(key) = &db.key {
            args["file"] = json!(key);
        }
        socket::try_call(route, tool, args)
    };
    let inventory = match call(ReadCommand::Components)? {
        Ok(inventory) => inventory,
        Err(e) => return Some(Err(e)),
    };
    let read = |ids: &[String]| -> Result<Vec<Value>, String> {
        ids.iter()
            .map(|id| {
                call(subtree_read(id))
                    .unwrap_or_else(|| Err("the running figmog serve went away; retry".into()))
            })
            .collect()
    };
    Some((|| {
        Ok(Fetched {
            icon_trees: read(&sources.icons)?,
            image_trees: read(&sources.images)?,
            inventory,
        })
    })())
}

/// One item of an images result as bytes, by its node id or imageRef.
fn blob(item: &images::ImageItem) -> (String, Result<Vec<u8>, String>) {
    let value = match &item.error {
        Some(e) => Err(e.clone()),
        None => Ok(item.bytes.clone()),
    };
    (item.subject.clone(), value)
}

fn fetch_direct(
    db: &Db,
    icon_ids: &[String],
    fill_nodes: &[String],
) -> Result<(Blobs, Blobs), String> {
    if icon_ids.is_empty() && fill_nodes.is_empty() {
        return Ok((Blobs::new(), Blobs::new()));
    }
    let mut st = db.open(|path| crate::open_store!(path))?;
    let token = match db.key {
        Some(_) => crate::auth::token()?,
        None => None,
    };
    let api = token.map(UreqApi::new);
    let api = api.as_ref().map(|a| a as &dyn FigmaApi);
    let download = crate::api::download_bytes;
    let mut renders = Blobs::new();
    let mut fills = Blobs::new();
    if !icon_ids.is_empty() {
        let scanned = st.rtx(|readers| {
            crate::with_scan_readers!(readers, |r| images::scan_split(
                &r,
                icon_ids,
                &[],
                "svg",
                1000,
                images::RenderPolicy::default(),
            ))
        });
        let items = images::resolve(
            &mut st,
            scanned,
            api,
            &download,
            db.key.as_deref(),
            icon_ids,
            "svg",
            None,
        );
        renders.extend(items.iter().filter(|i| i.kind == images::RENDER).map(blob));
    }
    if !fill_nodes.is_empty() {
        let scanned = st.rtx(|readers| {
            crate::with_scan_readers!(readers, |r| images::scan_split(
                &r,
                &[],
                fill_nodes,
                "png",
                1000,
                images::RenderPolicy::default(),
            ))
        });
        let items = images::resolve(
            &mut st,
            scanned,
            api,
            &download,
            db.key.as_deref(),
            &[],
            "png",
            None,
        );
        fills.extend(items.iter().filter(|i| i.kind == images::FILL).map(blob));
    }
    Ok((renders, fills))
}

fn fetch_routed(
    route: &socket::Route,
    db: &Db,
    icon_ids: &[String],
    fill_nodes: &[String],
) -> Result<(Blobs, Blobs), String> {
    let call = |args: Value| -> Result<Blobs, String> {
        let result = socket::try_images_call(route, args)
            .unwrap_or_else(|| Err("the running figmog serve went away; retry".into()))?;
        Ok(super::images::from_socket_result(result)?
            .into_iter()
            .filter_map(|f| {
                let subject = f.entry.get("id").or_else(|| f.entry.get("ref"))?;
                let subject = subject.as_str()?.to_string();
                let value = match (f.data, f.entry["error"].as_str()) {
                    (Some(bytes), None) => Ok(bytes),
                    (_, Some(e)) => Err(e.to_string()),
                    (None, None) => Err("too large to relay over the socket-routed tool call (>1MB) — stop `figmog serve` for this file, or use --no-socket, to fetch it directly".into()),
                };
                Some((subject, value))
            })
            .collect())
    };
    let renders = if icon_ids.is_empty() {
        Blobs::new()
    } else {
        call(json!({"ids": icon_ids, "format": "svg", "file": db.key}))?
    };
    let fills = if fill_nodes.is_empty() {
        Blobs::new()
    } else {
        call(json!({"ids": fill_nodes, "format": "png", "renders": false, "file": db.key}))?
    };
    Ok((renders, fills))
}

// ---- writing ----

/// Check `rel` (a project-relative path) for writing: no symlink and
/// nothing but directories on its way from `root`, not a symlink or a
/// non-file itself. Returns the existing bytes, if any.
fn check_target(root: &Path, rel: &str) -> Result<Option<Vec<u8>>, String> {
    check_project_path(rel).map_err(|e| format!("{rel}: {e}"))?;
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let mut path = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        let shown = parts[..=i].join("/");
        let last = i + 1 == parts.len();
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{shown} is a symlink — figmog writes assets only through real directories inside the project; remove the link or change [assets] in figmog.toml"
                ));
            }
            Ok(meta) if !last && !meta.is_dir() => {
                return Err(format!("{shown} is not a directory"));
            }
            Ok(meta) if last && !meta.is_file() => {
                return Err(format!("{shown} is not a regular file"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("checking {shown}: {e}")),
        }
    }
    std::fs::read(&path)
        .map(Some)
        .map_err(|e| format!("reading {rel}: {e}"))
}

/// Write `bytes` to `path` as a regular file, never through a symlink
/// swapped in after the check.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Check every planned file, then write the ones whose bytes differ.
/// Returns each path's status (`written` / `unchanged`).
fn write_files(
    root: &Path,
    plan: &Plan,
    force: bool,
) -> Result<BTreeMap<String, &'static str>, String> {
    let mut statuses = BTreeMap::new();
    for (rel, (kind, bytes)) in &plan.files {
        let status = match check_target(root, rel)? {
            None => "written",
            Some(existing) if existing == *bytes => "unchanged",
            Some(_) if force => "written",
            Some(existing) => match kind {
                Kind::Generated if assets::is_generated(&existing) => "written",
                Kind::Generated => {
                    return Err(format!(
                        "{rel} exists and was not generated by figmog (it does not start with figmog's generated header) — refusing to overwrite it; move it, change [assets] in figmog.toml, or pass --force"
                    ));
                }
                Kind::Content => {
                    return Err(format!(
                        "{rel} exists with other content than the image it is named after — refusing to overwrite it; move it, or pass --force"
                    ));
                }
            },
        };
        statuses.insert(rel.clone(), status);
    }
    for (rel, status) in &statuses {
        if *status != "written" {
            continue;
        }
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        // Directories created above are real; re-check before writing.
        check_target(root, rel)?;
        write_file(&path, &plan.files[rel].1).map_err(|e| format!("writing {rel}: {e}"))?;
    }
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn check_target_refuses_symlinks_anywhere_on_the_path() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        symlink(outside.path(), root.join("src/icons")).unwrap();
        let e = check_target(root, "src/icons/IconA.tsx").unwrap_err();
        assert!(e.starts_with("src/icons is a symlink"), "{e}");
        std::fs::write(outside.path().join("x"), "x").unwrap();
        symlink(outside.path().join("x"), root.join("src/file.tsx")).unwrap();
        let e = check_target(root, "src/file.tsx").unwrap_err();
        assert!(e.starts_with("src/file.tsx is a symlink"), "{e}");
        assert!(check_target(root, "../x").is_err());
        assert!(check_target(root, "/etc/passwd").is_err());
        std::fs::write(root.join("plain"), "p").unwrap();
        assert!(
            check_target(root, "plain/below")
                .unwrap_err()
                .contains("not a directory")
        );
        assert_eq!(check_target(root, "src/new/file").unwrap(), None);
        assert_eq!(check_target(root, "plain").unwrap(), Some(b"p".to_vec()));
    }
}
