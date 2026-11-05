//! `figmog map check | coverage` (NWS-2949): the project's
//! `figmog.map.json` ([`crate::map`]). `check` validates the file alone;
//! `coverage` is not a new read — the CLI maps it to the existing
//! `figmog_components` read ([`super::read_command`]), socket-routed or
//! opened directly like `figmog components`, and compares the map with
//! that inventory and the project's files here.
//!
//! Both read the discovered project's map file, so neither takes `--db`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::write_json;
use crate::dispatch::ReadCommand;
use crate::map::{CodeMap, MAP_FILE, coverage};
use crate::project::Layout;

#[derive(Subcommand)]
pub(super) enum MapCommand {
    /// Validate figmog.map.json (exit 1 naming the file and JSON path of
    /// the first error). Doesn't read the mirror.
    Check,
    /// Compare figmog.map.json with the mirror: Figma components and
    /// component sets with no map entry, map entries whose Figma component
    /// is not in the mirror, and whether each mapped code file exists.
    Coverage,
}

/// The read `coverage` runs: the mirror's design-system inventory.
pub(super) fn read() -> ReadCommand {
    ReadCommand::Components
}

/// `figmog map check`: `{map, components}` (the entry count), or the
/// validation error. A missing file is an error here.
pub(super) fn check(layout: &Layout) -> Result<(), String> {
    match CodeMap::load(&layout.project_root)? {
        Some(map) => write_json(&json!({"map": MAP_FILE, "components": map.entries.len()})),
        None => Err(format!(
            "no {MAP_FILE} at the project root ({})",
            layout.project_root.display()
        )),
    }
}

/// `figmog map coverage`, from the `figmog_components` answer. A missing
/// map file is an empty map (everything unmapped); an invalid one fails.
pub(super) fn run_coverage(inventory: &Value, layout: &Layout) -> Result<(), String> {
    let map = CodeMap::load(&layout.project_root)?;
    write_json(&coverage(map.as_ref(), inventory, &layout.project_root))
}
