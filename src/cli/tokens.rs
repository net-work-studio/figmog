//! `figmog tokens export` (NWS-2967). Not a new read: the CLI maps it to
//! the existing `figmog_vars {snapshot: true}` read ([`super::read_command`]),
//! which is socket-routed to a running serve or opened directly exactly
//! like `figmog vars --snapshot`; only the CSS rendering
//! ([`crate::tokens::export_css`]) and the writing happen here.

use std::io::Write;
use std::path::PathBuf;

use clap::Subcommand;
use serde_json::{Value, json};

use super::{STDOUT_CLOSED, write_json};
use crate::tokens::{ExportOptions, export_css};

#[derive(Subcommand)]
pub(super) enum TokensCommand {
    /// Write the variable snapshot as CSS custom properties, light/dark
    /// theme blocks and a Tailwind v4 `@theme` (to --out, else stdout).
    Export {
        /// Output file (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Collection whose modes become `:root` / `[data-theme="dark"]`
        /// (default: the one collection with a mode named like "dark").
        #[arg(long)]
        collection: Option<String>,
        /// Mode for `:root` (default: the mode named like "light", else the
        /// collection's default mode).
        #[arg(long)]
        light_mode: Option<String>,
        /// Mode for `[data-theme="dark"]` (default: the mode named like "dark").
        #[arg(long)]
        dark_mode: Option<String>,
    },
}

/// Render the `figmog_vars {snapshot: true}` result and write it. With
/// `--out`: the file, plus a JSON summary on stdout. Without: the CSS on
/// stdout and, when anything was reported, `{"report": [...]}` on stderr.
pub(super) fn write_export(command: &TokensCommand, vars: &Value) -> Result<(), String> {
    let TokensCommand::Export {
        out,
        collection,
        light_mode,
        dark_mode,
    } = command;
    let export = export_css(
        vars,
        &ExportOptions {
            collection: collection.clone(),
            light_mode: light_mode.clone(),
            dark_mode: dark_mode.clone(),
        },
    )?;
    match out {
        Some(path) => {
            std::fs::write(path, &export.css)
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
            write_json(&json!({
                "out": path.display().to_string(),
                "tokens": export.tokens,
                "theme": export.theme,
                "report": export.report,
            }))
        }
        None => {
            let mut stdout = std::io::stdout();
            if stdout.write_all(export.css.as_bytes()).is_err() || stdout.flush().is_err() {
                return Err(STDOUT_CLOSED.to_string());
            }
            if !export.report.is_empty() {
                eprintln!("{}", json!({"report": export.report}));
            }
            Ok(())
        }
    }
}

/// The read every `tokens` subcommand runs.
pub(super) fn read() -> crate::dispatch::ReadCommand {
    crate::dispatch::ReadCommand::Vars {
        id: None,
        snapshot: true,
    }
}
