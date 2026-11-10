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
        let recorded = previous
            .as_ref()
            .map(|p| p.sources.clone())
            .unwrap_or_default();
        // The recorded roots are node ids of the recorded file: never
        // replay them against another one.
        if let Some(file) = &recorded.file
            && db.key.as_deref() != Some(file.as_str())
        {
            return Err(format!(
                "{} was exported from Figma file {file}, but this project's file is {} — pass --icons/--images to export from it (replacing the manifest)",
                settings.manifest,
                db.key.as_deref().unwrap_or("unknown")
            ));
        }
        recorded
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
            file: db.key.clone(),
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
    let previous_paths: BTreeSet<String> = previous
        .iter()
        .flat_map(AssetIndex::generated_paths)
        .collect();
    let stale = stale(root, previous_paths.iter().map(String::as_str), &plan.files);
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
        let scanned = st.rtx(
            |((nodes, ..), _, _, _, _, _, meta, _, _, images_table, pins, _)| {
                images::scan_split(
                    &nodes,
                    &meta,
                    &images_table,
                    &pins,
                    icon_ids,
                    &[],
                    "svg",
                    1000,
                )
            },
        );
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
        let scanned = st.rtx(
            |((nodes, ..), _, _, _, _, _, meta, _, _, images_table, pins, _)| {
                images::scan_split(
                    &nodes,
                    &meta,
                    &images_table,
                    &pins,
                    &[],
                    fill_nodes,
                    "png",
                    1000,
                )
            },
        );
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

/// Files a previous export listed that this one doesn't write — except a
/// path that differs from a new one only in case (a renamed icon on a
/// case-insensitive filesystem is the same, live file).
fn stale<'a>(
    root: &Path,
    previous: impl Iterator<Item = &'a str>,
    files: &BTreeMap<String, (Kind, Vec<u8>)>,
) -> Vec<&'a str> {
    let live_casefolded: BTreeSet<String> = files.keys().map(|p| p.to_lowercase()).collect();
    previous
        .filter(|p| {
            if files.contains_key(*p) {
                return false;
            }
            if !live_casefolded.contains(&p.to_lowercase()) {
                return true;
            }
            // A differently spelled old path is live only if looking it
            // up actually aliases a new filename on this filesystem.
            let alias = check_target(root, p).ok().and_then(|e| e.alias);
            let actual = alias.map(|name| match p.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{name}"),
                None => name,
            });
            !actual.is_some_and(|actual| files.contains_key(&actual))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// How one path component exists in its directory.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Missing,
    /// An entry with exactly this name.
    Exact,
    /// Found only under another spelling (a case-insensitive filesystem):
    /// the name actually on disk.
    Alias(String),
}

/// Classify `name`, given whether looking it up found something and the
/// directory's actual entry names. Pure, so case-insensitive behavior is
/// testable on any filesystem.
fn classify(found: bool, entries: &[String], name: &str) -> Entry {
    if !found {
        return Entry::Missing;
    }
    if entries.iter().any(|e| e == name) {
        return Entry::Exact;
    }
    let lower = name.to_lowercase();
    match entries.iter().find(|e| e.to_lowercase() == lower) {
        Some(actual) => Entry::Alias(actual.clone()),
        // Found, but listed under no recognizable spelling (Unicode
        // normalization): report it as is, never guess.
        None => Entry::Alias(name.to_string()),
    }
}

fn entry_names(dir: &Path) -> Result<Vec<String>, String> {
    std::fs::read_dir(dir)
        .map_err(|e| format!("listing {}: {e}", dir.display()))?
        .map(|e| {
            e.map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("listing {}: {e}", dir.display()))
        })
        .collect()
}

/// What is on disk at a write target.
#[derive(Debug, Default, PartialEq, Eq)]
struct Existing {
    bytes: Option<Vec<u8>>,
    /// The file's name on disk when it differs from the target's only in
    /// case (it is renamed on write).
    alias: Option<String>,
}

/// Check `rel` (a project-relative path) for writing: no symlink and
/// nothing but directories on its way from `root`, not a symlink or a
/// non-file itself, and no directory spelled differently on disk.
fn check_target(root: &Path, rel: &str) -> Result<Existing, String> {
    check_project_path(rel).map_err(|e| format!("{rel}: {e}"))?;
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let mut path = root.to_path_buf();
    let mut alias = None;
    for (i, part) in parts.iter().enumerate() {
        let parent = path.clone();
        path.push(part);
        let shown = parts[..=i].join("/");
        let last = i + 1 == parts.len();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Existing::default());
            }
            Err(e) => return Err(format!("checking {shown}: {e}")),
        };
        if meta.file_type().is_symlink() {
            return Err(format!(
                "{shown} is a symlink — figmog writes assets only through real directories inside the project; remove the link or change [assets] in figmog.toml"
            ));
        }
        if !last && !meta.is_dir() {
            return Err(format!("{shown} is not a directory"));
        }
        if last && !meta.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        match classify(true, &entry_names(&parent)?, part) {
            Entry::Exact | Entry::Missing => {}
            Entry::Alias(actual) if last => alias = Some(actual),
            Entry::Alias(actual) => {
                return Err(format!(
                    "{shown} exists on disk as {actual} (a case-insensitive filesystem) — rename the directory or change [assets] in figmog.toml"
                ));
            }
        }
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {rel}: {e}"))?;
    Ok(Existing {
        bytes: Some(bytes),
        alias,
    })
}

/// Open a new file for writing: it must not exist, and is never reached
/// through a symlink.
fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// A name in `dir` nothing uses yet, for a temporary file.
fn temp_name(dir: &Path, name: &str, n: u32) -> std::path::PathBuf {
    dir.join(format!(".{name}.figmog-{}-{n}.tmp", std::process::id()))
}

/// Write `bytes` as `dir/name` atomically: a new temporary file in the
/// same directory, then renamed over the target (a rename replaces a
/// link, it never writes through one).
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    for n in 0..100 {
        let tmp = temp_name(dir, name, n);
        let mut file = match create_new(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let written = file
            .write_all(bytes)
            .and_then(|()| file.flush())
            .and_then(|()| std::fs::rename(&tmp, dir.join(name)));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return written;
    }
    Err(std::io::Error::other("no free temporary file name"))
}

/// Create `rel`'s directories under `root` one at a time, refusing any
/// symlink or non-directory on the way (never `create_dir_all`).
fn ensure_dirs(root: &Path, rel_dir: &[&str]) -> Result<(), String> {
    let mut path = root.to_path_buf();
    for (i, part) in rel_dir.iter().enumerate() {
        path.push(part);
        let shown = rel_dir[..=i].join("/");
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{shown} is a symlink — refusing to write through it"
                ));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{shown} is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    // Created meanwhile: checked again on the next pass.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let meta = std::fs::symlink_metadata(&path)
                            .map_err(|e| format!("checking {shown}: {e}"))?;
                        if meta.file_type().is_symlink() || !meta.is_dir() {
                            return Err(format!("{shown} is not a real directory"));
                        }
                    }
                    Err(e) => return Err(format!("creating {shown}: {e}")),
                }
            }
            Err(e) => return Err(format!("checking {shown}: {e}")),
        }
    }
    Ok(())
}

/// Check every planned file, then write the ones whose bytes (or
/// on-disk spelling) differ. Returns each path's status (`written` /
/// `unchanged`).
fn write_files(
    root: &Path,
    plan: &Plan,
    force: bool,
) -> Result<BTreeMap<String, &'static str>, String> {
    let mut statuses = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    for (rel, (kind, bytes)) in &plan.files {
        let existing = check_target(root, rel)?;
        let status = match existing.bytes {
            None => "written",
            Some(old) if old == *bytes && existing.alias.is_none() => "unchanged",
            Some(_) if force => "written",
            Some(old) if old == *bytes => "written",
            Some(old) => match kind {
                Kind::Generated if assets::is_generated(&old) => "written",
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
        if let Some(alias) = existing.alias {
            aliases.insert(rel.clone(), alias);
        }
        statuses.insert(rel.clone(), status);
    }
    for (rel, status) in &statuses {
        if *status != "written" {
            continue;
        }
        let parts: Vec<&str> = rel
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        let (name, dirs) = parts.split_last().expect("checked: a file path");
        ensure_dirs(root, dirs)?;
        let dir = dirs.iter().fold(root.to_path_buf(), |p, d| p.join(d));
        // Re-check now that the directories exist.
        check_target(root, rel)?;
        // A case-only rename: move the old spelling aside first, so the
        // new file gets the new name on a case-insensitive filesystem.
        let aside = match aliases.get(rel) {
            Some(alias) => {
                let aside = (0..100)
                    .map(|n| temp_name(&dir, name, 1000 + n))
                    .find(|p| std::fs::symlink_metadata(p).is_err())
                    .ok_or_else(|| format!("renaming {rel}: no free temporary name"))?;
                std::fs::rename(dir.join(alias), &aside)
                    .map_err(|e| format!("renaming {alias} for {rel}: {e}"))?;
                Some(aside)
            }
            None => None,
        };
        write_atomic(&dir, name, &plan.files[rel].1).map_err(|e| format!("writing {rel}: {e}"))?;
        if let Some(aside) = aside {
            std::fs::remove_file(&aside)
                .map_err(|e| format!("removing the old spelling of {rel}: {e}"))?;
        }
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
        assert_eq!(
            check_target(root, "src/new/file").unwrap(),
            Existing::default()
        );
        assert_eq!(
            check_target(root, "plain").unwrap(),
            Existing {
                bytes: Some(b"p".to_vec()),
                alias: None
            }
        );
        // ensure_dirs never creates through a symlink.
        let e = ensure_dirs(root, &["src", "icons", "svg"]).unwrap_err();
        assert!(e.starts_with("src/icons is a symlink"), "{e}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    /// Case-insensitive filesystems, simulated: a lookup of `IconStar.tsx`
    /// that finds `Iconstar.tsx` is an alias, never an exact match.
    #[test]
    fn classify_spots_case_aliases() {
        let entries = vec!["Iconstar.tsx".to_string(), "index.ts".to_string()];
        assert_eq!(classify(false, &entries, "IconStar.tsx"), Entry::Missing);
        assert_eq!(classify(true, &entries, "index.ts"), Entry::Exact);
        assert_eq!(
            classify(true, &entries, "IconStar.tsx"),
            Entry::Alias("Iconstar.tsx".into())
        );
        assert_eq!(
            classify(true, &entries, "Odd.tsx"),
            Entry::Alias("Odd.tsx".into())
        );
    }

    #[test]
    fn case_differences_are_not_assumed_to_alias() {
        let dir = tempfile::tempdir().unwrap();
        let files = [(
            "src/icons/IconStar.tsx".to_string(),
            (Kind::Generated, vec![]),
        )]
        .into();
        assert_eq!(
            stale(
                dir.path(),
                ["src/icons/Iconstar.tsx", "src/icons/IconGone.tsx"].into_iter(),
                &files
            ),
            ["src/icons/IconGone.tsx", "src/icons/Iconstar.tsx"]
        );
    }

    /// A case-only rename on a real directory: the old spelling is moved
    /// aside and replaced, whatever the filesystem's case sensitivity.
    #[test]
    fn write_files_renames_a_case_alias_and_writes_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("icons")).unwrap();
        let old = format!("{}old", assets::js_header());
        std::fs::write(root.join("icons/Iconstar.tsx"), &old).unwrap();
        let new = format!("{}new", assets::js_header()).into_bytes();
        let mut files = BTreeMap::new();
        files.insert(
            "icons/IconStar.tsx".to_string(),
            (Kind::Generated, new.clone()),
        );
        let plan = Plan {
            files,
            icons: vec![],
            images: vec![],
            skipped: vec![],
        };
        let statuses = write_files(root, &plan, false).unwrap();
        assert_eq!(statuses["icons/IconStar.tsx"], "written");
        let names = entry_names(&root.join("icons")).unwrap();
        // Case-sensitive: both spellings are separate files (the old one
        // is left for `stale`); case-insensitive: only the new spelling.
        assert!(names.contains(&"IconStar.tsx".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
        let stale_paths = stale(root, ["icons/Iconstar.tsx"].into_iter(), &plan.files);
        if names.contains(&"Iconstar.tsx".to_string()) {
            assert_eq!(stale_paths, ["icons/Iconstar.tsx"]);
        } else {
            assert!(stale_paths.is_empty(), "{stale_paths:?}");
        }
        assert_eq!(std::fs::read(root.join("icons/IconStar.tsx")).unwrap(), new);
        // Unchanged bytes: nothing written.
        assert_eq!(
            write_files(root, &plan, false).unwrap()["icons/IconStar.tsx"],
            "unchanged"
        );
    }
}
