//! `figmog tokens export | diff | check` (NWS-2967, NWS-2948). Not new
//! reads: the CLI maps each to the existing `figmog_vars {snapshot: true}`
//! read ([`super::read_command`]), which is socket-routed to a running
//! serve or opened directly exactly like `figmog vars --snapshot`; only the
//! CSS rendering and comparison ([`crate::tokens`]) and the file handling
//! happen here.
//!
//! Settings come from the discovered project ([`Layout::settings`]): the
//! token file is `[stack] tokens`, the format `[tokens] format`, the mode
//! mapping `[tokens.modes]`; `--format` and `--mode` override them. With
//! `--db` there is no project: defaults apply, `export` writes to stdout
//! unless `--out` names a file, and `diff`/`check` need `--file`.

use std::io::Write;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::{STDOUT_CLOSED, write_json};
use crate::project::Layout;
use crate::tokens::{CERTIFIES, ExportOptions, Format, diff, export_css, parse_mode_arg};

#[derive(Subcommand)]
pub(super) enum TokensCommand {
    /// Write the variable snapshot as CSS custom properties, one block per
    /// mode selector and (format `tailwind`) a Tailwind v4 `@theme` — to
    /// figmog.toml's `[stack] tokens` file, --out, or stdout.
    Export {
        /// Output file (default: figmog.toml's `[stack] tokens`; stdout
        /// with --db). `-` is stdout.
        #[arg(long, conflicts_with = "stdout")]
        out: Option<PathBuf>,
        /// Write the CSS to stdout instead of the token file.
        #[arg(long)]
        stdout: bool,
        #[command(flatten)]
        render: RenderArgs,
    },
    /// List the tokens added, removed or changed between the committed
    /// token file and the variable snapshot in the mirror.
    Diff {
        /// The committed token file (default: figmog.toml's `[stack] tokens`).
        #[arg(long)]
        file: Option<PathBuf>,
        #[command(flatten)]
        render: RenderArgs,
    },
    /// Exit 1 unless the committed token file matches the last published
    /// snapshot (the one in the mirror — not the live Figma file).
    Check {
        /// The committed token file (default: figmog.toml's `[stack] tokens`).
        #[arg(long)]
        file: Option<PathBuf>,
        #[command(flatten)]
        render: RenderArgs,
    },
}

/// How the snapshot is rendered: figmog.toml's `[tokens]`, overridable.
#[derive(Args)]
pub(super) struct RenderArgs {
    /// `tailwind` (CSS variables plus a Tailwind v4 `@theme`) or `css`
    /// (CSS variables only). Default: figmog.toml's `[tokens] format`,
    /// else tailwind.
    #[arg(long)]
    format: Option<String>,
    /// Map a collection's mode to a selector, over figmog.toml's
    /// `[tokens.modes]`: `<collection>/<mode>=<selector>`, `:root` for the
    /// root mode, empty to leave the mode out. Repeatable.
    #[arg(long = "mode", value_name = "COLLECTION/MODE=SELECTOR")]
    modes: Vec<String>,
}

impl RenderArgs {
    fn options(&self, layout: Option<&Layout>) -> Result<ExportOptions, String> {
        let settings = layout.map(Layout::settings).unwrap_or_default();
        let format = match &self.format {
            Some(f) => Format::parse(f).map_err(|e| format!("--format: {e}"))?,
            None => settings.tokens.format,
        };
        let mut modes = settings.tokens.modes;
        for arg in &self.modes {
            let (key, selector) = parse_mode_arg(arg)?;
            modes.insert(key, selector);
        }
        Ok(ExportOptions { format, modes })
    }
}

/// The token file: `explicit`, else the project's `[stack] tokens`. The
/// second value is how it is named in output.
fn token_file(explicit: Option<&PathBuf>, layout: Option<&Layout>) -> Option<(PathBuf, String)> {
    if let Some(path) = explicit {
        return Some((path.clone(), path.display().to_string()));
    }
    let layout = layout?;
    let stack = layout.settings().stack;
    Some((stack.tokens_path(&layout.project_root), stack.tokens))
}

/// The snapshot provenance `check` and `diff` print (NWS-2961).
fn provenance(vars: &Value) -> Value {
    let s = &vars["snapshot"];
    json!({
        "source": s["source"],
        "format": s["format"],
        "format_version": s["format_version"],
        "exported_at": s["exported_at"],
        "digest": s["digest"],
    })
}

/// Run `command` on the `figmog_vars {snapshot: true}` result.
pub(super) fn run(
    command: &TokensCommand,
    vars: &Value,
    layout: Option<&Layout>,
) -> Result<(), String> {
    match command {
        TokensCommand::Export {
            out,
            stdout,
            render,
        } => {
            let export = export_css(vars, &render.options(layout)?)?;
            let to_stdout = *stdout || out.as_deref().is_some_and(|p| p.as_os_str() == "-");
            let target = if to_stdout {
                None
            } else {
                token_file(out.as_ref(), layout)
            };
            let Some((path, _)) = target else {
                // Stdout: the CSS itself, and any report as one stderr line.
                let mut stdout = std::io::stdout();
                if stdout.write_all(export.css.as_bytes()).is_err() || stdout.flush().is_err() {
                    return Err(STDOUT_CLOSED.to_string());
                }
                if !export.report.is_empty() {
                    eprintln!("{}", json!({"report": export.report}));
                }
                return Ok(());
            };
            // Unchanged bytes are left alone (no mtime churn).
            let status = match std::fs::read(&path) {
                Ok(existing) if existing == export.css.as_bytes() => "unchanged",
                _ => {
                    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
                    }
                    std::fs::write(&path, &export.css)
                        .map_err(|e| format!("writing {}: {e}", path.display()))?;
                    "written"
                }
            };
            write_json(&json!({
                "out": path.display().to_string(),
                "status": status,
                "format": export.format.as_str(),
                "tokens": export.tokens,
                "theme": export.theme,
                "modes": export.modes,
                "report": export.report,
            }))
        }
        TokensCommand::Diff { file, render } | TokensCommand::Check { file, render } => {
            let check = matches!(command, TokensCommand::Check { .. });
            let (path, shown) = token_file(file.as_ref(), layout).ok_or_else(|| {
                "--db has no project to take `[stack] tokens` from — pass --file <committed token file>".to_string()
            })?;
            let export = export_css(vars, &render.options(layout)?)?;
            let committed = match std::fs::read_to_string(&path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("reading {}: {e}", path.display())),
            };
            let d = diff(committed.as_deref(), &export)
                .map_err(|e| format!("{shown} is not a token file figmog can compare: {e}"))?;
            if check && d.status == "up_to_date" {
                return write_json(&json!({
                    "status": "ok",
                    "file": shown,
                    "certifies": CERTIFIES,
                    "result": format!("{shown} {CERTIFIES}"),
                    "snapshot": provenance(vars),
                    "note": "Compared with the variable snapshot in this mirror (provenance above), not with the live Figma file: changes published in Figma after that snapshot are seen only once it is re-imported (`figmog import-variables`) or re-pulled.",
                }));
            }
            let mut out = serde_json::to_value(&d).map_err(|e| e.to_string())?;
            out["file"] = json!(shown);
            out["snapshot"] = provenance(vars);
            write_json(&out)?;
            if !check {
                return Ok(());
            }
            Err(if d.status == "missing" {
                format!(
                    "{shown} does not exist — run `figmog tokens export` to write it from the last published snapshot"
                )
            } else {
                format!(
                    "{shown} does not match the last published snapshot ({}) — run `figmog tokens export` to regenerate it",
                    d.summary()
                )
            })
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
