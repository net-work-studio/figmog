//! Command-line surface. Read commands never touch the network: they open
//! the local store and read one snapshot.
//!
//! Split into `pull` (pull/do_pull/`PullError`/`.figmog/current` helpers)
//! and `call` (`tools`/`call`/`import-variables`, the desktop-proxy CLI
//! parity surface). This module keeps the clap types, top-level dispatch,
//! `run`, and the store-opening/JSON-writing helpers every submodule
//! shares. The read-only query commands (`status`, `pages`, ... `at`) are
//! converted to the shared `ReadCommand` (`read_command`) and either
//! executed directly or sent over the socket in its tool form.

use std::path::{Component, Path, PathBuf};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::dispatch::{DEFAULT_SEARCH_LIMIT, ReadCommand};
use crate::ident::{is_file_key, parse_file_ref};
use crate::project::Layout;

mod assets;
mod auth;
mod call;
mod doctor;
mod gitignore;
mod images;
mod init;
mod map;
mod migrate;
mod pull;
mod socket;
mod store_lock;
mod tokens;

pub(crate) use store_lock::Compaction;
use store_lock::{OwnedStore, StoreOwner};

#[cfg(test)]
pub(crate) use pull::apply_pull_at;
pub(crate) use pull::{PullError, now_ms, pull_failure_wait, write_current};
#[cfg(test)]
pub(crate) use socket::{write_over_local, write_over_stream};

#[derive(Parser)]
#[command(
    name = "figmog",
    version,
    about = "fold-backed local mirror of a Figma file"
)]
struct Cli {
    /// Store directory (default: the file's mirror under the mirror root —
    /// `<git-common-dir>/figmog/<file-key>/db` inside Git, else
    /// `.figmog/<file-key>/db`; see `figmog.toml` in the README).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Don't try the running serve's unix-socket control plane (spec §1):
    /// every read command, `tools`, and `call` always open the store
    /// directly, exactly like pre-v0.0.2 figmog. On `figmog serve`, don't
    /// listen on the socket at all.
    #[arg(long, global = true)]
    no_socket: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the installed figmog version.
    Version,
    /// Save, inspect, or remove your Figma credentials.
    Auth {
        #[command(subcommand)]
        command: auth::AuthCommand,
    },
    /// Fetch the file (or read a saved response) and sync the mirror.
    Pull {
        /// File key or figma.com URL. Optional after the first pull.
        file: Option<String>,
        /// Ingest a saved GET /v1/files/:key response instead of the network.
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Wipe the store and rebuild from scratch.
        #[arg(long)]
        fresh: bool,
        /// Request vector path data (fillGeometry/strokeGeometry) on this
        /// pull. Sticky (spec §4): once set, every later pull of this
        /// mirror keeps requesting it until `--fresh`.
        #[arg(long)]
        geometry: bool,
    },
    /// MCP stdio server: `figmog_*` tools over the local mirror, with the
    /// sync loop built in (one process owns the store), plus (unless
    /// `--no-upstream`) an uncached proxy to Figma's native desktop MCP
    /// server — figmog is the only Figma MCP an agent needs to connect.
    Serve {
        /// File keys or figma.com URLs to mirror at startup — zero or
        /// more (spec §14: the server starts empty and mirrors files as
        /// agents reference them). The first one is the default file for
        /// any tool call that omits `file`.
        files: Vec<String>,
        /// Poll interval in seconds. Clamped to a one-day maximum.
        #[arg(long, default_value = "10")]
        interval: u64,
        /// Disable the poll loop (offline/fixture use).
        #[arg(long)]
        no_watch: bool,
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// Serve local `figmog_*` tools only — no upstream proxy.
        #[arg(long)]
        no_upstream: bool,
        /// Root directory for multi-file session stores (`<root>/<key>/db`
        /// — spec §14) and the control socket. Hidden: testability knob so
        /// e2e tests can point startup files at pre-built fixture stores
        /// under a temp dir instead of the discovered mirror root.
        #[arg(long, hide = true)]
        figmog_root: Option<PathBuf>,
        /// Run as the repository's shared background owner (NWS-2953).
        /// Hidden: started detached by `figmog serve` itself, never by hand.
        #[arg(long, hide = true, conflicts_with_all = ["figmog_root", "files"])]
        background: bool,
    },
    /// Set up this repository in one step: write `figmog.toml` (detecting
    /// React and Tailwind), save the token, keep `.figmog/` out of Git,
    /// move old mirrors to the shared root, register the MCP server for
    /// Claude Code (`.mcp.json`), add figmog's block to `AGENTS.md`, and
    /// run the first pull. Never prompts; safe to rerun.
    Init(init::InitArgs),
    /// Check the token, file access, the plan's variables access, the
    /// desktop MCP connection, the MCP registration and the dev server.
    /// Exits 1 when any check fails.
    Doctor {
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
    },
    /// Print the project's settings (`figmog.toml` over defaults).
    Settings,
    /// Move old per-checkout mirrors (`.figmog/<key>/`) into the shared
    /// mirror root (inside Git: `<git-common-dir>/figmog/`). Resumable;
    /// never overwrites an existing mirror; fails while a store is open.
    Migrate {
        /// File key or figma.com URL (default: every old mirror found).
        file: Option<String>,
    },
    /// List every tool figmog would serve: the local registry, plus
    /// upstream tools when reachable.
    Tools {
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// List local `figmog_*` tools only — no upstream probe.
        #[arg(long)]
        no_upstream: bool,
    },
    /// Invoke any tool by name through the same dispatch `figmog serve`
    /// uses — local `figmog_*` tools included.
    Call {
        tool: String,
        /// JSON object of arguments (default `{}`).
        #[arg(long)]
        args: Option<String>,
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// Don't probe upstream — fail on a non-`figmog_*` tool name.
        #[arg(long)]
        no_upstream: bool,
    },
    /// File name, version, last modified, node count.
    Status,
    /// List pages.
    Pages,
    /// Subtree outline (default: whole document).
    Tree {
        id: Option<String>,
        #[arg(long)]
        depth: Option<usize>,
    },
    /// Full raw JSON of one node.
    Get {
        id: String,
        #[arg(long)]
        children: bool,
        /// Annotate `boundVariables` binding sites with variable names and
        /// per-mode values under a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
    },
    /// Full raw JSON subtree dump rooted at a node (default depth: unlimited).
    Dump {
        id: String,
        #[arg(long)]
        depth: Option<usize>,
        /// Project every node to these raw fields (id/name/type/children
        /// always survive). Comma-separated, e.g. --fields id,name,fills.
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
        /// Annotate `boundVariables` binding sites with variable names and
        /// per-mode values under a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
    },
    /// Nodes by type, optionally within one page and/or a subtree scope.
    Find {
        #[arg(long = "type")]
        node_type: String,
        #[arg(long)]
        page: Option<String>,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
    },
    /// BM25 search over layer names and text content.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
    },
    /// Instances of a component (by node id, key, or name).
    Instances { target: String },
    /// Design-system inventory: sets, variant axes, standalone components.
    Components,
    /// Styles with usage counts; --values derives definitions from consumers.
    Styles {
        #[arg(long = "type")]
        style_type: Option<String>,
        #[arg(long)]
        values: bool,
        /// With --values, annotate the definition's variable bindings under
        /// a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
    },
    /// Nodes using a style id or bound to a variable id.
    Uses { id: String },
    /// Variables: authoritative if imported, else inferred from bindings.
    Vars {
        id: Option<String>,
        /// Return `{snapshot, variables}`: the variable snapshot's state
        /// and provenance next to the rows.
        #[arg(long)]
        snapshot: bool,
    },
    /// Import a variables export (REST or plugin-console shape), replacing
    /// the previous variable snapshot.
    ImportVariables {
        path: PathBuf,
        /// Permit an empty export to replace the stored variable snapshot.
        #[arg(long)]
        allow_empty: bool,
        /// Validate and report the replacement without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Design tokens from the imported variable snapshot.
    Tokens {
        #[command(subcommand)]
        command: tokens::TokensCommand,
    },
    /// The project's Figma → code component map (figmog.map.json).
    Map {
        #[command(subcommand)]
        command: map::MapCommand,
    },
    /// Icons and images exported into the project (figmog.toml's
    /// `[assets]`), with the manifest `figmog spec` references.
    Assets {
        #[command(subcommand)]
        command: assets::AssetsCommand,
    },
    /// Node counts by type and page, table totals, text-node count, max tree depth.
    Stats,
    /// Ancestor chain root→node for one id.
    Path { id: String },
    /// Every TEXT node's (id, characters, page_id), optionally scoped to one page.
    Text {
        #[arg(long)]
        page: Option<String>,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
    },
    /// Nodes whose raw JSON matches an RFC 6901 pointer, optionally by value.
    Where {
        /// RFC 6901 pointer into the node's raw JSON, e.g. /layoutMode.
        #[arg(long)]
        pointer: String,
        /// JSON value to match; parsed as JSON, falling back to a bare string.
        #[arg(long)]
        equals: Option<String>,
        #[arg(long)]
        page: Option<String>,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
    },
    /// Visible nodes on one page containing a point, sorted by area then id.
    At {
        #[arg(long, value_parser = finite_coordinate)]
        x: f64,
        #[arg(long, value_parser = finite_coordinate)]
        y: f64,
        /// Page id or Figma page URL defining the coordinate space.
        #[arg(long)]
        page: String,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
        /// Maximum number of matches after filtering and sorting.
        #[arg(long, default_value_t = crate::query::DEFAULT_AT_LIMIT)]
        limit: usize,
    },
    /// Code-ready spec of a subtree: per node the recorded design layout
    /// and figmog's CSS interpretation, each value as its bound token
    /// (with the `tokens export` CSS name) or flagged untokenized, text,
    /// instances and assets; untranslatable cases listed as unsupported.
    Spec {
        id: String,
        /// Add a Tailwind class hint per node (default in a project: on
        /// when `[stack] styling` is `tailwind`; with --db: off).
        #[arg(long, conflicts_with = "no_tailwind")]
        tailwind: bool,
        /// Leave the Tailwind class hint out.
        #[arg(long)]
        no_tailwind: bool,
    },
    /// Download node renders and/or fill images to --out, caching by file version.
    ///
    /// With --db, export cached images only: stores do not record a file key.
    /// To fetch misses, run `figmog pull <file-key-or-url>`, then
    /// `figmog images <node-id>...` without --db. A node URL selects a node
    /// within the chosen store, not the store's file identity.
    /// Never automatic — the images endpoints are Figma's most rate-limited.
    Images {
        /// Node ids (or Figma URLs carrying node-id=) to render, and/or
        /// scan for fill imageRefs. At least one required.
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long, default_value = "png")]
        format: String,
        /// Render scale (default 1).
        #[arg(long)]
        scale: Option<f64>,
        /// Output directory (default ./figmog-images/).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// Non-finite coordinates cannot round-trip through JSON (they become
/// `null`), so reject them consistently before direct or socket dispatch.
fn finite_coordinate(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| "expected a finite number".to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("expected a finite number".into())
    }
}

/// Sentinel [`dispatch`]/[`write_json`] error string meaning "stdout
/// closed mid-write, not a command failure" (the reader vanished — piped
/// into `head`, a killed consumer, …). Never printed: `run` recognizes it
/// and maps it to a silent, clean exit 0 instead of the ordinary exit-1
/// `{"error": ...}` path, the same "vanished client = clean shutdown"
/// stance `serve.rs`'s stdout frame writes take for the MCP loop.
const STDOUT_CLOSED: &str = "\0figmog:stdout-closed";

/// Parse `argv`, dispatch, and return the process exit code (0 on success,
/// 1 with `{"error": <message>}` on stderr otherwise — except a closed
/// stdout (the internal `STDOUT_CLOSED` sentinel), which is a silent 0).
pub fn run() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => 0,
        Err(e) if e == STDOUT_CLOSED => 0,
        Err(e) => {
            eprintln!("{}", json!({"error": e}));
            1
        }
    }
}

/// The one place every command writes its JSON result to stdout (spec §4:
/// JSON is the CLI's only output mode). `println!`/`print!` panic on a
/// write error (their internal `.expect`) — and the overwhelmingly likely
/// error here is the reader having gone away (piped into `head`, a killed
/// consumer, a closed terminal), not a real figmog fault. Writes manually
/// so that case becomes [`STDOUT_CLOSED`] instead: a clean, expected exit
/// 0 rather than a panic or a broken-pipe error message the operator can't
/// act on. Serialization failure (a genuine bug — `v` came from `serde_json`
/// itself, so this should never happen for real) stays a normal exit-1
/// error.
fn write_json(v: &Value) -> Result<(), String> {
    use std::io::Write;
    let text = crate::deep_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{text}").is_err() || stdout.flush().is_err() {
        return Err(STDOUT_CLOSED.to_string());
    }
    Ok(())
}

fn dispatch(cli: Cli) -> Result<(), String> {
    if let Cmd::Version = &cli.cmd {
        return write_json(&json!({"version": env!("CARGO_PKG_VERSION")}));
    }
    if let Cmd::Auth { command } = &cli.cmd {
        return auth::run(command);
    }
    // `init` and `doctor` discover the project themselves: `init` may be
    // creating its config, and `doctor` reports a malformed one as a check.
    if let Cmd::Init(_) | Cmd::Doctor { .. } = &cli.cmd
        && cli.db.is_some()
    {
        return Err("init and doctor work on the project and do not take --db".into());
    }
    if let Cmd::Init(args) = cli.cmd {
        return write_json(&init::cmd_init(args, cli.no_socket)?);
    }
    if let Cmd::Doctor { upstream } = &cli.cmd {
        let (report, failed) = doctor::cmd_doctor(upstream);
        write_json(&report)?;
        if failed {
            let names: Vec<&str> = report["checks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["status"] == "fail")
                .filter_map(|c| c["name"].as_str())
                .collect();
            return Err(format!("doctor: failed checks: {}", names.join(", ")));
        }
        return Ok(());
    }
    // `serve` manages its own (possibly many) session stores via
    // `SessionManager` (`sessions.rs`) rather than the single `Db` every
    // other command resolves below — handled here, before `resolve_db`,
    // matched by reference so a non-match leaves `cli` untouched for the
    // rest of this function. The global `--db` flag is still honored as a
    // single-session escape hatch (spec §14 non-goal: CLI multi-file
    // addressing is out of scope, and this keeps every pre-v4 `figmog serve
    // --db <path>` invocation — including this crate's own e2e tests —
    // working unchanged, single mirror, no `--figmog-root` layout involved).
    if let Cmd::Serve {
        files,
        interval,
        no_watch,
        upstream,
        no_upstream,
        figmog_root,
        background,
    } = &cli.cmd
    {
        if *background {
            if cli.db.is_some() || cli.no_socket {
                return Err("--background takes neither --db nor --no-socket".into());
            }
            let settings = crate::owner::Settings {
                watch: !no_watch,
                interval: *interval,
                upstream: (!no_upstream).then(|| upstream.clone()),
            };
            return crate::serve::run_owner(Layout::discover_cwd()?, settings);
        }
        // The default shape — no `--db`, `--figmog-root` or `--no-socket`
        // — relays stdio to the repository's background owner, starting it
        // if needed (NWS-2953). The others serve in this process.
        if cli.db.is_none() && figmog_root.is_none() && !cli.no_socket {
            let settings = crate::owner::Settings {
                watch: !no_watch,
                interval: *interval,
                upstream: (!no_upstream).then(|| upstream.clone()),
            };
            return crate::serve::run_relay(Layout::discover_cwd()?, files, settings);
        }
        // `--db` serves one fixed store: no project discovery at all.
        let layout = match cli.db {
            Some(_) => None,
            None => Some(Layout::discover_cwd()?),
        };
        return crate::serve::run_serve(
            cli.db.clone(),
            files.clone(),
            *interval,
            *no_watch,
            upstream.clone(),
            *no_upstream,
            figmog_root.clone(),
            layout,
            cli.no_socket,
        );
    }

    // Every other command without `--db` works inside the discovered
    // project (spec §16); a malformed `figmog.toml` fails here, clearly.
    let layout = match cli.db {
        Some(_) => None,
        None => Some(Layout::discover_cwd()?),
    };

    if let Cmd::Settings = &cli.cmd {
        let Some(layout) = &layout else {
            return Err("settings reads the project's figmog.toml and does not take --db".into());
        };
        return write_json(&init::cmd_settings(layout));
    }

    // `map` reads the project's figmog.map.json: no project, no map.
    if let Cmd::Map { command } = &cli.cmd {
        let Some(layout) = &layout else {
            return Err(format!(
                "map reads the project's {} and does not take --db",
                crate::map::MAP_FILE
            ));
        };
        if let map::MapCommand::Check = command {
            return map::check(layout);
        }
    }

    // `assets` writes into the project: no project, nowhere to write.
    if let Cmd::Assets { .. } = &cli.cmd
        && layout.is_none()
    {
        return Err(
            "assets writes into the project's [assets] folders and does not take --db".into(),
        );
    }

    if let Cmd::Migrate { file } = &cli.cmd {
        let Some(layout) = &layout else {
            return Err("migrate works on the project's mirrors and does not take --db".into());
        };
        let key = file
            .as_deref()
            .map(|f| parse_file_ref(f).ok_or_else(|| format!("not a Figma file key or URL: {f}")))
            .transpose()?;
        let report = migrate::cmd_migrate(layout, key)?;
        write_json(&report)?;
        return if report["failed"]
            .as_array()
            .is_some_and(|failed| !failed.is_empty())
        {
            Err("some mirrors could not be migrated; see the failed entries in the migration report".into())
        } else {
            Ok(())
        };
    }

    // `figmog tools` never reads the store — it only needs an upstream
    // probe (spec §4 debt item M5), so it's dispatched here, before
    // `resolve_db`, exactly like `serve` above: it works with no
    // established `.figmog/current` and no `--db` at all, unlike `figmog
    // call` (below), which does need a resolved mirror to open.
    //
    // Socket routing (spec §1) applies here too, before any direct probe:
    // reachable ⇒ the *running serve's* own merged registry (its own
    // upstream connection, not this invocation's `--upstream`/
    // `--no-upstream` — see `socket::try_tools_list`'s doc comment)
    // answers instead.
    if let Cmd::Tools {
        upstream,
        no_upstream,
    } = &cli.cmd
    {
        if !cli.no_socket
            && let Some(layout) = &layout
            && let Some(result) = socket::try_tools_list(&socket::Route::new(layout))
        {
            let tools = result?;
            note_ignored_upstream_flags(upstream, *no_upstream);
            return write_json(&socket::tools_rows(&tools));
        }
        return call::cmd_tools(upstream.clone(), *no_upstream);
    }

    if let Cmd::Call { tool, args, .. } = &cli.cmd {
        let args = crate::deep_json::OwnedValue(call::parse_args(args.as_deref())?);
        call::validate_local_args(tool, &args)?;
    }

    // Socket routing (spec §1): every read command plus `call` first tries
    // the running serve's unix-socket control plane for the resolved
    // figmog-root — reachable ⇒ the command becomes a client of serve
    // (always-fresh, no lock, and `call figmog_sync` reaches the *owning*
    // process); unreachable ⇒ falls through to the direct-open path below,
    // unchanged. Only meaningful when the root is knowable at all: `--db`
    // bypasses the whole multi-file root concept (an arbitrary store path
    // has no `<root>/serve.sock` to speak of), so this is skipped whenever
    // `cli.db` was given, exactly like `--no-socket`.
    if !cli.no_socket
        && let Some(layout) = &layout
    {
        let root = &socket::Route::new(layout);
        if let Cmd::Call {
            tool,
            args,
            upstream,
            no_upstream,
        } = &cli.cmd
        {
            let parsed_args = call::parse_args(args.as_deref())?;
            // Only per-file local tools use mirror routing. Desktop
            // arguments and management calls pass through unchanged.
            let parsed_args = if is_per_file_local_tool(tool) {
                with_current_file(parsed_args, layout)?
            } else {
                parsed_args
            };
            if let Some(route) = route_for(root, layout, &parsed_args, is_per_file_local_tool(tool))
                && let Some(result) = socket::try_call(&route, tool, parsed_args)
            {
                note_ignored_upstream_flags(upstream, *no_upstream);
                return write_json(&crate::deep_json::OwnedValue(result?));
            }
        } else if let Some(read) = read_command(&cli.cmd) {
            // The file this read targets: what the socket call carries.
            let target = with_current_file(read.to_tool_call().1, layout)?
                .get("file")
                .and_then(Value::as_str)
                .and_then(parse_file_ref)
                .or_else(|| crate::dispatch::infer_file_from_node_ref(&read.to_tool_call().1));
            let (tool, tool_args) =
                with_project_settings(read, Some(layout), target.as_deref())?.to_tool_call();
            let tool_args = with_current_file(tool_args, layout)?;
            if let Some(route) = route_for(root, layout, &tool_args, true)
                && let Some(result) = socket::try_call(&route, tool, tool_args)
            {
                let result = result?;
                // Not ready is an error, except for an import-only store
                // (SPEC §9) with no pull attempted. Once a pull is in
                // progress or failed, report its lifecycle before stripping it.
                if tool == "figmog_status"
                    && result["ready"] == false
                    && (!has_variable_state(&result) || !result["pull"]["job"].is_null())
                {
                    if let Some(error) = result["pull"]["last_completed"]["error"].as_str() {
                        return Err(format!(
                            "mirror is not ready — pull failed: {error}; inspect `figmog call figmog_status` or `figmog call figmog_files` for pull progress and errors"
                        ));
                    }
                    return Err("mirror is not ready — inspect `figmog call figmog_status` or `figmog call figmog_files` for pull progress and errors".to_string());
                }
                let result = if tool == "figmog_status" {
                    strip_routed_status_fields(result)
                } else {
                    result
                };
                let result = crate::deep_json::OwnedValue(result);
                return write_read(&cli.cmd, &result, Some(layout));
            }
        }
    }

    let socket_root = match (&layout, cli.no_socket) {
        (Some(layout), false) => Some(socket::Route::new(layout)),
        _ => None,
    };
    let db = resolve_db(&cli, layout)?;
    // A mirror still in this checkout's old location is never the shared
    // owner's (it serves only the shared mirror root, NWS-2953): only an
    // in-process serve's socket is tried for it, else it is opened directly.
    let socket_root = socket_root.map(|root| match (&db.layout, &db.key) {
        (Some(layout), Some(key)) if layout.legacy_store(key, &db.path).is_some() => {
            root.local_only()
        }
        _ => root,
    });
    // Writers through a running serve (NWS-2963): when its socket answers,
    // serve owns (or will open) this store, so `pull` and
    // `import-variables` become its operations — same output, no lock
    // error. Unreachable ⇒ the direct path below. `--db` and `--no-socket`
    // never route (no `socket_root`).
    if let (Some(root), Some(key)) = (&socket_root, &db.key)
        && matches!(cli.cmd, Cmd::Pull { .. } | Cmd::ImportVariables { .. })
        && let Some(client) = socket::connect(root)
    {
        let client = client?;
        return match cli.cmd {
            Cmd::Pull {
                from_file,
                fresh,
                geometry,
                ..
            } => pull::cmd_pull_routed(&db, key, root, client, from_file, fresh, geometry),
            Cmd::ImportVariables {
                path,
                allow_empty,
                dry_run,
            } => call::cmd_import_variables_routed(
                &db,
                root,
                client,
                key,
                path,
                allow_empty,
                dry_run,
            ),
            _ => unreachable!("matched above"),
        };
    }
    match cli.cmd {
        Cmd::Pull {
            file,
            from_file,
            fresh,
            geometry,
        } => pull::cmd_pull(&db, file, from_file, fresh, geometry),
        Cmd::ImportVariables {
            path,
            allow_empty,
            dry_run,
        } => call::cmd_import_variables(&db, path, allow_empty, dry_run),
        Cmd::Images {
            ids,
            format,
            scale,
            out,
        } => images::cmd_images(&db, socket_root.as_ref(), ids, format, scale, out),
        Cmd::Assets { command } => assets::run(&db, socket_root.as_ref(), &command),
        Cmd::Call {
            tool,
            args,
            upstream,
            no_upstream,
        } => call::cmd_call(&db, tool, args, upstream, no_upstream),
        other => {
            let Some(read) = read_command(&other) else {
                unreachable!("every non-read command is handled above")
            };
            let read = with_project_settings(read, db.layout.as_ref(), db.key.as_deref())?;
            // `open_store!`'s pipeline type contains fn items and can't be
            // named, so the store must be opened at this concrete call
            // site; the reader tuple itself is nameable (`RootReaders`), so
            // the query runs through the same `ReadCommand::execute` arm
            // every tool adapter uses (NWS-2902).
            let st = db.open_existing(|path| crate::open_store!(path))?;
            let result = crate::deep_json::OwnedValue(st.rtx(|r| read.execute(&r))?);
            drop(st);
            write_read(&other, &result, db.layout.as_ref())
        }
    }
}

/// `figmog spec` names tokens exactly as the project's token file does:
/// the project's `[tokens]` settings ride along as `figmog_spec`'s
/// `tokens` argument (NWS-2948), and its `[stack] styling` turns on the
/// Tailwind class hint (NWS-2947) unless `--tailwind`/`--no-tailwind`
/// said otherwise; its `figmog.map.json`, when there is one, rides along
/// as `map` (NWS-2949) — an invalid map fails the command — and its asset
/// manifest (`[assets] manifest`), when there is one, as `assets`
/// (NWS-2952), likewise, for the file `target` (its icons match by node id
/// only when they were exported from that file). With `--db` (no
/// project): the defaults, no hint, no map, no manifest.
fn with_project_settings(
    read: ReadCommand,
    layout: Option<&Layout>,
    target: Option<&str>,
) -> Result<ReadCommand, String> {
    Ok(match read {
        ReadCommand::Spec {
            id,
            tokens,
            tailwind,
            map,
            assets,
        } => {
            let settings = layout.map(Layout::settings);
            let map = match (map, layout) {
                (Some(map), _) => Some(map),
                (None, Some(layout)) => crate::map::CodeMap::load(&layout.project_root)?,
                (None, None) => None,
            };
            let assets = match (assets, layout, &settings) {
                (Some(assets), _, _) => Some(assets),
                (None, Some(layout), Some(settings)) => crate::assets::AssetIndex::load(
                    &layout.project_root,
                    &settings.assets.manifest,
                )?
                .map(|index| index.for_file(target)),
                _ => None,
            };
            ReadCommand::Spec {
                id,
                tokens: tokens.or_else(|| settings.as_ref().map(|s| s.tokens.export_options())),
                tailwind: tailwind.or_else(|| {
                    settings
                        .as_ref()
                        .map(|s| s.stack.styling == crate::project::Styling::Tailwind)
                }),
                map,
                assets,
            }
        }
        other => other,
    })
}

/// Print a read command's result: JSON for every read, except `tokens`,
/// which renders its `figmog_vars` read as CSS and compares it with the
/// project's token file (NWS-2967, NWS-2948), and `map coverage`, which
/// compares its `figmog_components` read with the project's map (NWS-2949).
fn write_read(cmd: &Cmd, result: &Value, layout: Option<&Layout>) -> Result<(), String> {
    match cmd {
        Cmd::Tokens { command } => tokens::run(command, result, layout),
        Cmd::Map { .. } => match layout {
            Some(layout) => map::run_coverage(result, layout),
            None => unreachable!("`map` without a project fails before any read"),
        },
        _ => write_json(result),
    }
}

/// Review m5: when a `tools`/`call` invocation explicitly named an
/// `--upstream`/`--no-upstream` but got routed to the running serve's own
/// socket, that flag was silently ignored (serve's own upstream connection,
/// established at its own startup, is what actually answers — see
/// `socket::try_tools_list`'s doc comment). Rather than leave that silent,
/// print one stderr note. Detecting "explicitly passed" without clap's
/// `ArgMatches` introspection is approximate: `no_upstream` is unambiguous
/// (there's no way to "explicitly pass false" for a plain flag), but
/// `upstream` can only be compared against its own default value — a user
/// who explicitly retyped that exact default URL goes unnoted. An
/// acceptable, documented gap for an informational note, not a behavior
/// difference.
fn note_ignored_upstream_flags(upstream: &str, no_upstream: bool) {
    if no_upstream || upstream != crate::serve::DEFAULT_UPSTREAM_URL {
        eprintln!(
            "figmog: --upstream/--no-upstream ignored — routed to the running serve, whose own upstream settings apply"
        );
    }
}

/// Prefer an explicit `file` over a pasted node URL, matching serve's
/// session routing. Validation of an explicit file ref happens when the
/// chosen mirror is resolved; an invalid explicit ref never falls back.
/// A present-but-non-string `file` is a type error (NWS-2944), never a
/// silent fall-through to the project default; `null` counts as omitted.
fn requested_file(args: &Value) -> Result<Option<String>, String> {
    Ok(crate::dispatch::arg_str(args, "file")?
        .or_else(|| crate::dispatch::infer_file_from_node_ref(args)))
}

/// Management tools don't target an existing mirror, and desktop tools
/// use the desktop's own context rather than local mirror routing.
pub(crate) fn is_per_file_local_tool(tool: &str) -> bool {
    crate::proxy::is_local_tool(tool)
        && !matches!(
            tool,
            "figmog_open" | "figmog_files" | crate::serve::writes::OPERATION
        )
}

/// For per-file local tools, inject the project default's key
/// ([`Layout::default_key`]: `figmog.toml`'s `[files] default`, else
/// `.figmog/current`) as the routed `file` argument when one is established, no explicit non-null `file`
/// was supplied, and no node URL selects a file (review C1): without this,
/// a socket-routed call carries no `file` at all, so serve answers from
/// *its own* default session (`SessionManager::effective_default_key`) —
/// which can silently diverge
/// from the mirror the project default (and thus direct mode) actually
/// targets, or, in the zero-startup-file quick-start shape, doesn't exist
/// at all (serve has no sessions yet), producing a hard "no default
/// mirrored file" error even though a mirror is clearly established on
/// disk. Injecting the current key routes the call through
/// `SessionManager::resolve`'s explicit-`file` path instead, which
/// auto-opens that session if serve hasn't touched it yet — a real
/// Tier-1 pull only if the on-disk store isn't already mirrored (an
/// already-pulled store never spends one; see `sessions::open_session_at`'s
/// `mirrored: meta_present`). No project default ⇒ args unchanged: an
/// in-process serve keeps its own-default behavior, while the shared
/// background owner is not asked at all ([`route_for`]).
///
/// Never applied to desktop tools, `figmog_files`, or operation receipt
/// queries, which have no mirror routing, nor to `figmog_open`: its own
/// `file` argument names the file *to open*, not a routing hint — it's required, and direct mode's own
/// dispatch correctly rejects an omitted one; silently defaulting it here
/// would let `figmog call figmog_open`
/// (no `--args`) succeed over the socket while the exact same invocation
/// fails direct, breaking the very byte-compatibility this routing exists
/// to preserve (see `read_command`'s socket caller).
fn with_current_file(args: Value, layout: &Layout) -> Result<Value, String> {
    let mut args = crate::deep_json::OwnedValue(args);
    if args.is_null() {
        args.0 = json!({});
    }
    // Preserve explicit non-null values for the local tool's own argument
    // handling, even when they cannot select a mirror.
    let has_file = args.get("file").is_some_and(|value| !value.is_null());
    let url_file = crate::dispatch::infer_file_from_node_ref(&args);
    if let Some(obj) = args.0.as_object_mut()
        && !has_file
        && url_file.is_none()
        && let Some(key) = layout.default_key()?
    {
        obj.insert("file".to_string(), json!(key));
    }
    Ok(args.into_inner())
}

/// Where a routed call may go. The shared background owner (NWS-2953)
/// is skipped — leaving only an in-process serve's local socket, else the
/// direct path — when the call's file mirror resolves to this checkout's
/// old per-checkout location (the owner never serves it), and when a
/// per-file call names no file this worktree resolves: the owner holds
/// every worktree's files and has no default, so the call must not land on
/// another worktree's; the direct path reports this worktree's "no mirror
/// here" instead. Management and desktop calls (`per_file` false) without
/// a file route anywhere.
///
/// `None` — no socket at all — when the file's mirror does not resolve
/// ([`Layout::active_store`] errs, e.g. two mirrors or an interrupted
/// migrate): no serve may answer for a store the direct path refuses, so
/// the caller falls through to it and it reports that error, exactly as
/// with `--no-socket` (NWS-2953, NWS-2964).
fn route_for(
    root: &socket::Route,
    layout: &Layout,
    args: &Value,
    per_file: bool,
) -> Option<socket::Route> {
    let key = args
        .get("file")
        .and_then(Value::as_str)
        .and_then(parse_file_ref)
        .or_else(|| crate::dispatch::infer_file_from_node_ref(args));
    let shared = match key {
        Some(key) => !layout.active_store(&key).ok()?.migration_pending,
        None => !per_file,
    };
    Some(if shared {
        root.clone()
    } else {
        root.local_only()
    })
}

/// Whether a status result carries a variable snapshot summary with
/// stored state (anything but `none`) — the import-only status case.
fn has_variable_state(status: &Value) -> bool {
    status["variables"]["state"]
        .as_str()
        .is_some_and(|state| state != "none")
}

/// Preserve the direct read command's JSON shape for an existing mirror.
/// `figmog call figmog_status` keeps the full server lifecycle instead.
fn strip_routed_status_fields(mut result: Value) -> Value {
    if let Some(obj) = result.as_object_mut() {
        obj.remove("upstream");
        obj.remove("ready");
        obj.remove("pull");
    }
    result
}

/// Maps a read-only [`Cmd`] to its typed [`ReadCommand`] (NWS-2902) —
/// the one representation the direct path executes and the socket path
/// serializes ([`ReadCommand::to_tool_call`]), so a socket-routed read
/// produces byte-identical results to the direct-open path (spec §6a).
/// `None` for every other command: the writers `pull` and
/// `import-variables` route as serve operations (NWS-2963, in `dispatch`);
/// `serve`/`tools`/`call`/`images`/`assets` are each handled at their own
/// call sites.
fn read_command(cmd: &Cmd) -> Option<ReadCommand> {
    Some(match cmd {
        Cmd::Status => ReadCommand::Status,
        Cmd::Pages => ReadCommand::Pages,
        Cmd::Tree { id, depth } => ReadCommand::Tree {
            id: id.clone(),
            depth: *depth,
        },
        Cmd::Get {
            id,
            children,
            resolve_vars,
        } => ReadCommand::Node {
            id: id.clone(),
            children: *children,
            resolve_vars: *resolve_vars,
        },
        Cmd::Dump {
            id,
            depth,
            fields,
            resolve_vars,
        } => ReadCommand::Subtree {
            id: id.clone(),
            depth: *depth,
            fields: fields.clone(),
            resolve_vars: *resolve_vars,
        },
        Cmd::Find {
            node_type,
            page,
            under,
        } => ReadCommand::Find {
            node_type: node_type.clone(),
            page: page.clone(),
            under: under.clone(),
        },
        Cmd::Search {
            query,
            limit,
            under,
        } => ReadCommand::Search {
            query: query.clone(),
            limit: *limit,
            under: under.clone(),
        },
        Cmd::Instances { target } => ReadCommand::Instances {
            target: target.clone(),
        },
        Cmd::Components => ReadCommand::Components,
        Cmd::Styles {
            style_type,
            values,
            resolve_vars,
        } => ReadCommand::Styles {
            style_type: style_type.clone(),
            values: *values,
            resolve_vars: *resolve_vars,
        },
        Cmd::Uses { id } => ReadCommand::Uses { id: id.clone() },
        Cmd::Vars { id, snapshot } => ReadCommand::Vars {
            id: id.clone(),
            snapshot: *snapshot,
        },
        Cmd::Stats => ReadCommand::Stats,
        Cmd::Path { id } => ReadCommand::Path { id: id.clone() },
        Cmd::Text { page, under } => ReadCommand::Text {
            page: page.clone(),
            under: under.clone(),
        },
        Cmd::Where {
            pointer,
            equals,
            page,
            under,
        } => ReadCommand::Where {
            pointer: pointer.clone(),
            equals: equals.as_deref().map(parse_equals),
            page: page.clone(),
            under: under.clone(),
        },
        Cmd::At {
            x,
            y,
            page,
            under,
            limit,
        } => ReadCommand::At {
            x: *x,
            y: *y,
            page: page.clone(),
            under: under.clone(),
            limit: *limit,
        },
        Cmd::Spec {
            id,
            tailwind,
            no_tailwind,
        } => ReadCommand::Spec {
            id: id.clone(),
            tokens: None,
            tailwind: match (tailwind, no_tailwind) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            },
            map: None,
            assets: None,
        },
        Cmd::Tokens { .. } => tokens::read(),
        Cmd::Map { .. } => map::read(),
        Cmd::Version
        | Cmd::Auth { .. }
        | Cmd::Pull { .. }
        | Cmd::Serve { .. }
        | Cmd::Tools { .. }
        | Cmd::Call { .. }
        | Cmd::ImportVariables { .. }
        | Cmd::Images { .. }
        | Cmd::Assets { .. }
        | Cmd::Migrate { .. }
        | Cmd::Init(_)
        | Cmd::Doctor { .. }
        | Cmd::Settings => return None,
    })
}

/// `--equals <json>`: parse as JSON, falling back to treating the bare word
/// as a JSON string (so `--equals VERTICAL` works without quoting). A
/// literal `--equals null` is equality to null (NWS-2897).
fn parse_equals(raw: &str) -> String {
    let value = crate::deep_json::from_str(raw)
        .unwrap_or_else(|_| crate::deep_json::OwnedValue(Value::String(raw.to_string())));
    crate::deep_json::to_string(&*value).expect("JSON value serializes")
}

// ---- config / db resolution ----

/// The store to open plus (when known) the file key it mirrors and the
/// project it belongs to (`None` exactly when `--db` was given).
struct Db {
    path: PathBuf,
    key: Option<String>,
    layout: Option<Layout>,
}

impl Db {
    /// Open this command's store: [`open_store_resolved`], so an old
    /// per-checkout store moved by a concurrent `figmog migrate` is never
    /// recreated.
    fn open<T>(
        &self,
        open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
    ) -> Result<OwnedStore<T>, String> {
        let legacy = match (&self.layout, &self.key) {
            (Some(layout), Some(key)) => layout.legacy_store(key, &self.path),
            _ => None,
        };
        open_store_resolved(&self.path, legacy.as_ref(), open)
    }
    /// Open a read destination without initializing a missing mirror.
    fn open_existing<T>(
        &self,
        open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
    ) -> Result<OwnedStore<T>, String> {
        let legacy = match (&self.layout, &self.key) {
            (Some(layout), Some(key)) => layout.legacy_store(key, &self.path),
            _ => None,
        };
        open_store_existing_resolved(&self.path, legacy.as_ref(), open)
    }
}

fn resolve_db(cli: &Cli, layout: Option<Layout>) -> Result<Db, String> {
    let Some(layout) = layout else {
        let path = cli.db.clone().expect("only --db skips project discovery");
        return Ok(Db {
            path,
            key: None,
            layout: None,
        });
    };

    // pull with an explicit file ref establishes the key for this run.
    // `.figmog/current` is only written after a successful sync (see
    // `pull::do_pull`), so a failed pull never repoints later commands.
    // `serve` never reaches here — it's handled, `Db`-free, before this
    // function is even called (see `dispatch`).
    //
    // Otherwise a per-file local call's explicit file wins over URL
    // inference; the project default (`figmog.toml`, then
    // `.figmog/current`) is only a fallback. Desktop calls keep the
    // default mirror regardless of their arguments, since they cannot
    // select the desktop's open file. Reuse the read command's tool
    // arguments so `id`, `under`, and `target` follow the socket/server
    // URL inference. `figmog_open` and `figmog_files` keep their
    // management semantics.
    let file = match &cli.cmd {
        Cmd::Pull { file: Some(f), .. } => Some(f.clone()),
        Cmd::Call { tool, args, .. } if is_per_file_local_tool(tool) => {
            let args = crate::deep_json::OwnedValue(call::parse_args(args.as_deref())?);
            requested_file(&args)?
        }
        cmd => match read_command(cmd) {
            Some(read) => {
                let args = crate::deep_json::OwnedValue(read.to_tool_call().1);
                requested_file(&args)?
            }
            None => None,
        },
    };
    let key = match file {
        Some(file) => {
            parse_file_ref(&file).ok_or_else(|| format!("not a Figma file key or URL: {file}"))?
        }
        None => layout.default_key()?.ok_or_else(|| no_mirror_msg(cli))?,
    };
    check_key(&key)?;
    let store = layout.active_store(&key)?;
    if store.migration_pending {
        eprintln!(
            "{}",
            crate::project::migration_pending_warning(&key, &store)
        );
    }
    Ok(Db {
        path: store.path,
        key: Some(key),
        layout: Some(layout),
    })
}

/// `pull --from-file` with neither a file ref nor an established key has
/// nothing to sync into — point the user at `--from-file`'s own
/// requirements rather than the generic "run pull first" message (which
/// would tell a user already running pull to run pull).
fn no_mirror_msg(cli: &Cli) -> String {
    if let Cmd::Pull {
        from_file: Some(_),
        file: None,
        ..
    } = &cli.cmd
    {
        "--from-file needs a target mirror: pass the file key/url too, or --db <path>".into()
    } else {
        "no mirror here — run `figmog pull <file-url>` first".into()
    }
}

/// Keys must stay one normal component at the path-construction boundary,
/// even if a future caller bypasses file-reference or persisted-key parsing.
fn check_key(key: &str) -> Result<(), String> {
    let mut components = Path::new(key).components();
    if !is_file_key(key)
        || !matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        )
    {
        return Err("invalid mirror key: expected a single bare Figma file key".to_string());
    }
    Ok(())
}

/// The clean, user-facing error every CLI store-opening call site below
/// translates a locked-store panic into (I-1). `figmog serve` holds fjall's
/// single-writer lock for the life of the process — a CLI command opening
/// the same `--db` concurrently must not surface fold's raw `unwrap()`
/// panic (exit 101).
const STORE_LOCKED_MSG: &str = "store is locked — is `figmog serve` running? Query the server instead (figmog call/tools), or stop it first.";

/// This process's owner record, written into every store lock it takes
/// (NWS-2963). Set once by `figmog serve`; other commands leave it unset
/// and record nothing.
static OWNER_RECORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Make this process (a `figmog serve`) name itself in the store locks it
/// takes, so a command that finds one held — `--db`, `--no-socket`, a
/// serve in another checkout — can say who owns it instead of a bare
/// lock error. `socket` is where it listens, if anywhere.
pub(crate) fn record_store_owner(socket: Option<&Path>) {
    let socket = socket.map_or_else(
        || "no socket".to_string(),
        |path| {
            let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            format!("socket {}", path.display())
        },
    );
    let _ = OWNER_RECORD.set(format!(
        "figmog serve (pid {}, {socket})",
        std::process::id()
    ));
}

/// The locked-store error naming its owner (a running serve).
fn store_owned_msg(owner: &str) -> String {
    format!(
        "store is locked by {owner} — run the command without --db/--no-socket from that serve's project to route it through the serve, or stop it first"
    )
}

/// Establish a private directory at `path` before invoking `open`, including
/// on existing mirrors. Failure to protect it is a clean error and never
/// reaches fold, which may write journal data while opening the store.
/// `open` receives the verified absolute path and must use it for the store.
/// Unsafe ancestor ownership or write permissions fail before the callback.
/// The returned handle also holds a permanent in-store lock file for its
/// entire lifetime, so `pull --fresh` cannot unlink a live writer's store.
/// Opens and fresh resets need no write access to the store's parent.
///
/// `open_store!` (via `fold::stream::Stream::new`) panics rather than
/// returning a `Result` when the underlying store can't be opened — most
/// commonly because another process (`figmog serve`) already holds fjall's
/// single-writer lock (`fjall::Error::Locked`). fold itself stays untouched
/// (its panic-on-open contract is intentional and shared by `wtx`'s own
/// rollback-on-panic path); this wrapper is figmog's layer, catching that
/// one specific panic and translating it into a clean exit-1 `Err` instead.
///
/// **stderr is JSON on every path — including a panic.** The default panic
/// hook is swapped for a no-op for the duration of the call (restored
/// immediately after, on both outcomes) so a lock panic can never print
/// fold's raw `thread 'main' panicked… ` banner ahead of our own JSON line;
/// an earlier version left the hook active specifically to preserve that
/// banner, which broke the "stderr always parses as one JSON object"
/// contract every other error path in this CLI honors. A *non*-lock panic
/// (a genuine bug — corrupt store, disk error, anything unrelated to lock
/// contention) is no longer re-raised via `resume_unwind`, since with the
/// hook suppressed that would exit 101 with **no** stderr output at all —
/// silently swallowing it is worse than the old raw-panic banner was.
/// Instead its payload message is extracted (falling back to `"unknown
/// panic"` for a non-string payload) and emitted as our own
/// `{"error": "internal panic: ..."}` JSON line, then the process exits
/// 101 directly (distinct from the ordinary exit-1 `Err` path, so a caller
/// can still tell "this command failed" from "this command's process
/// itself came apart"). One deliberate cost: this sacrifices Rust's
/// backtrace — `RUST_BACKTRACE=1` has nothing to print through a
/// suppressed hook — traded for a stderr contract every consumer (this
/// CLI's own tests included) can rely on unconditionally.
///
/// **Not safe to race across threads within one test binary:** the swapped
/// panic hook is process-global, not per-call — `cargo test`'s default
/// parallel test execution means a *different* test's unrelated panic,
/// happening on another thread while this function's hook is installed,
/// would also hit the no-op hook and print no banner (its own assertion
/// failure still fails that test; only the diagnostic banner is at risk).
/// This crate's own suite has never hit that in practice (the window is a
/// handful of instructions around one `catch_unwind` call), but a test
/// added here that deliberately panics on another thread concurrently
/// should account for it — e.g. by running serially (`--test-threads=1`)
/// or accepting a possibly-missing banner rather than treating one as a
/// hard requirement.
pub(crate) fn open_store_checked<T>(
    path: &Path,
    open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
) -> Result<OwnedStore<T>, String> {
    open_store_owned(StoreOwner::acquire(path)?, open)
}

/// [`open_store_checked`] for a resolved store (NWS-2964). A store in the
/// old per-checkout location (`legacy`) is never created: its directory
/// must still exist when locked, and once owned it must still be the
/// active store — otherwise a `figmog migrate` moved it after resolution,
/// and the command fails asking for a rerun (which resolves the shared
/// store) instead of recreating the old one beside the moved mirror.
pub(crate) fn open_store_resolved<T>(
    path: &Path,
    legacy: Option<&crate::project::LegacyStore>,
    open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
) -> Result<OwnedStore<T>, String> {
    let Some(legacy) = legacy else {
        return open_store_checked(path, open);
    };
    let owner = StoreOwner::acquire_existing(path)?.ok_or_else(|| legacy.moved_msg())?;
    // Holding the anchor excludes a migration of this store (it takes the
    // same anchor first and keeps it until the old copy is gone).
    legacy.still_active()?;
    open_store_owned(owner, open)
}

/// Read-only admission: missing stores never acquire a newly created database.
pub(crate) fn open_store_existing_resolved<T>(
    path: &Path,
    legacy: Option<&crate::project::LegacyStore>,
    open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
) -> Result<OwnedStore<T>, String> {
    let owner = StoreOwner::acquire_existing(path)?
        .ok_or_else(|| "no mirror here — run `figmog pull <file-url>` first".to_string())?;
    if let Some(legacy) = legacy {
        legacy.still_active()?;
    }
    open_store_owned(owner, open)
}

/// Open the store at `path` emptied (`pull --fresh` through a running
/// serve, NWS-2963): like [`OwnedStore::reset`] — the first open checks
/// fjall's own lock too — but returns the cleared store opened by `open`
/// under the same ownership, with no window for another opener.
pub(crate) fn open_store_cleared<T>(
    path: &Path,
    legacy: Option<&crate::project::LegacyStore>,
    open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
) -> Result<OwnedStore<T>, String> {
    let owner = open_store_resolved(path, legacy, |path| crate::open_store!(path))?.into_owner();
    owner.clear_contents()?;
    open_store_owned(owner, open)
}

fn open_store_owned<T>(
    owner: StoreOwner,
    open: impl FnOnce(&Path) -> T + std::panic::UnwindSafe,
) -> Result<OwnedStore<T>, String> {
    let path = crate::private_dir::ensure(&owner.path)?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| open(&path));
    std::panic::set_hook(prev_hook);

    let store = result.map_err(|payload| {
        let msg = panic_message(&*payload);
        if msg.contains("Locked") {
            STORE_LOCKED_MSG.to_string()
        } else {
            eprintln!("{}", json!({"error": format!("internal panic: {msg}")}));
            std::process::exit(101);
        }
    })?;
    Ok(OwnedStore::new(store, owner))
}

/// Best-effort text for a caught panic's payload: `&str`/`String` cover
/// every panic this codebase (and fold) actually raises (`panic!`,
/// `.expect(...)`, `.unwrap()`), falling back to `"unknown panic"` for
/// anything else (a non-string payload, reachable in principle via
/// `std::panic::panic_any`). Split out from [`open_store_checked`] so it's
/// unit-testable without touching that function's `std::process::exit`
/// path — which itself can't be exercised by an in-process test (calling
/// it for real would tear down the whole `cargo test` binary, not just one
/// test); that path is proven by inspection and the doc comment above.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I-1: a store opened a second time in-process while the first handle
    /// is still held reproduces the exact panic a CLI command hits against
    /// a running `figmog serve` (fjall's file lock conflicts
    /// on the second `File::try_lock`, regardless of whether the two opens
    /// are in the same process or different ones — see
    /// `fjall::locked_file::LockedFileGuard`). `open_store_checked` must
    /// translate that panic into the clean, exit-1-friendly message instead
    /// of letting it propagate as a raw panic.
    #[test]
    fn open_store_checked_translates_locked_store_panic_to_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");

        // Hold the first handle open, exactly like `figmog serve` does for
        // the life of its process.
        let _held = crate::open_store!(&db_path);

        let result = open_store_checked(&db_path, |path| crate::open_store!(path));
        assert_eq!(result.err().as_deref(), Some(STORE_LOCKED_MSG));
    }

    /// The happy path: no contention, no panic, the store opens normally.
    #[test]
    fn open_store_checked_passes_through_a_successful_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        let result = open_store_checked(&db_path, |path| crate::open_store!(path));
        assert!(result.is_ok());
    }

    /// Synchronize an attacker at the old post-protection/path-open window.
    /// A shared non-sticky parent lets it replace even a 0700 child. The
    /// safe outcome is to reject that parent before this closure can run.
    #[test]
    fn open_store_checked_rejects_replaceable_paths_before_the_open_callback() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        use std::sync::Barrier;

        let work = tempfile::tempdir().unwrap();
        let shared = work.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let db = shared.join("db");
        std::fs::DirBuilder::new().mode(0o700).create(&db).unwrap();
        let result = open_store_checked(&db, |path| {
            let barrier = Barrier::new(2);
            std::thread::scope(|scope| {
                let attacker = scope.spawn(|| {
                    barrier.wait();
                    std::fs::rename(&db, shared.join("displaced")).unwrap();
                    std::fs::DirBuilder::new().mode(0o777).create(&db).unwrap();
                    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o777)).unwrap();
                });
                barrier.wait();
                attacker.join().unwrap();
                // Represents the subsequent path-based journal open. It
                // must never be reached for this replaceable pathname.
                std::fs::write(path.join("synthetic.jnl"), b"private synthetic payload").unwrap();
            });
        });
        assert!(
            result.is_err(),
            "unsafe path reached the store-open callback"
        );
        assert!(!db.join("synthetic.jnl").exists());
    }

    /// `panic_message` is what tells a lock panic (reworded to
    /// `STORE_LOCKED_MSG`) apart from any other panic (emitted verbatim as
    /// `internal panic: <msg>` and exit 101 — see `open_store_checked`'s
    /// doc comment) — covering the `&str` and `String` payload shapes a
    /// real `panic!`/`.unwrap()` produces, plus the `"unknown panic"`
    /// fallback for a payload that's neither.
    #[test]
    fn panic_message_extracts_str_and_string_payloads_and_falls_back() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*str_payload), "boom");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*string_payload), "boom");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*other_payload), "unknown panic");
    }

    // ---- socket-routing helpers (review C1/C2/m1 fix round) ----

    #[test]
    fn resolve_db_safely_discards_deep_equals_arguments() {
        crate::test_support::small_stack(|| {
            let equals = format!("{}null{}", r#"{"":["#.repeat(15_000), "]}".repeat(15_000));
            let cli = Cli::try_parse_from([
                "figmog",
                "--no-socket",
                "where",
                "--pointer",
                "/id",
                "--equals",
                &equals,
                "--under",
                "https://www.figma.com/file/SyntheticFileKey123/Test?node-id=0-0",
            ])
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let layout = Layout::discover(dir.path()).unwrap();
            let expected = layout.mirror_root.join("SyntheticFileKey123/db");
            let db = resolve_db(&cli, Some(layout)).unwrap();
            assert_eq!(db.key.as_deref(), Some("SyntheticFileKey123"));
            assert_eq!(db.path, expected);
        });
    }

    #[test]
    fn strip_routed_status_fields_removes_only_that_key() {
        let result = strip_routed_status_fields(json!({
            "name": "Fixture",
            "nodes": 12,
            "upstream": "connected",
        }));
        assert_eq!(result, json!({"name": "Fixture", "nodes": 12}));
    }

    #[test]
    fn strip_routed_status_fields_is_a_no_op_when_absent() {
        let result = json!({"name": "Fixture", "nodes": 12});
        assert_eq!(strip_routed_status_fields(result.clone()), result);
    }

    #[test]
    fn with_current_file_never_overrides_an_explicit_non_null_file() {
        // No filesystem dependency either way: an explicit `file` short-
        // circuits before the project default is ever consulted.
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::discover(dir.path()).unwrap();
        for file in [
            json!("explicitkey12345678"),
            json!({"path": "synthetic-file"}),
            json!(42),
            json!(false),
            json!([]),
        ] {
            let args = json!({"id": "1:1", "file": file});
            assert_eq!(with_current_file(args.clone(), &layout).unwrap(), args);
        }
    }

    #[test]
    fn at_rejects_non_finite_coordinates_before_dispatch() {
        for coordinate in ["NaN", "inf", "-inf", "1e400"] {
            for axis in ["--x", "--y"] {
                let value = format!("{axis}={coordinate}");
                let other = if axis == "--x" { "--y=1" } else { "--x=1" };
                let error = Cli::try_parse_from(["figmog", "at", &value, other, "--page", "0:1"])
                    .err()
                    .unwrap();
                assert!(
                    error.to_string().contains("expected a finite number"),
                    "{error}"
                );
            }
        }
    }

    /// NWS-2902: every clap read command's socket form parses back to the
    /// same typed command the direct path executes, and the socket JSON
    /// keeps its pre-refactor shape (absent optionals omitted, flags and
    /// `limit` always sent, `equals` present exactly when given).
    #[test]
    fn read_commands_round_trip_through_their_tool_form() {
        let cases: &[(&[&str], &str, Value)] = &[
            (&["status"], "figmog_status", json!({})),
            (&["tree"], "figmog_tree", json!({})),
            (
                &["tree", "1:1", "--depth", "2"],
                "figmog_tree",
                json!({"id": "1:1", "depth": 2}),
            ),
            (
                &["get", "1:1"],
                "figmog_node",
                json!({"id": "1:1", "children": false, "resolve_vars": false}),
            ),
            (
                &["dump", "1:1", "--fields", "id,fills", "--depth", "0"],
                "figmog_subtree",
                json!({"id": "1:1", "depth": 0, "fields": ["id", "fills"], "resolve_vars": false}),
            ),
            (
                &["find", "--type", "TEXT", "--under", "1:1"],
                "figmog_find",
                json!({"type": "TEXT", "under": "1:1"}),
            ),
            (
                &["find", "--type", "TEXT", "--page", "0:1"],
                "figmog_find",
                json!({"type": "TEXT", "page": "0:1"}),
            ),
            (
                &["search", "x"],
                "figmog_search",
                json!({"query": "x", "limit": 10}),
            ),
            (
                &["styles", "--type", "FILL"],
                "figmog_styles",
                json!({"type": "FILL", "values": false, "resolve_vars": false}),
            ),
            (&["vars"], "figmog_vars", json!({"snapshot": false})),
            (
                &["text", "--page", "0:1"],
                "figmog_text",
                json!({"page": "0:1"}),
            ),
            (
                &["where", "--pointer", "/a"],
                "figmog_where",
                json!({"pointer": "/a"}),
            ),
            (
                &["where", "--pointer", "/a", "--equals", "null"],
                "figmog_where",
                json!({"pointer": "/a", "equals": null}),
            ),
            (
                &["where", "--pointer", "/a", "--equals", "VERTICAL"],
                "figmog_where",
                json!({"pointer": "/a", "equals": "VERTICAL"}),
            ),
            (
                &[
                    "where",
                    "--pointer",
                    "/a",
                    "--equals",
                    "VERTICAL",
                    "--page",
                    "0:1",
                ],
                "figmog_where",
                json!({"pointer": "/a", "equals": "VERTICAL", "page": "0:1"}),
            ),
            (
                &["text", "--page", "0:1", "--under", "1:1"],
                "figmog_text",
                json!({"page": "0:1", "under": "1:1"}),
            ),
            (
                &["at", "--x", "1.5", "--y", "2", "--page", "0:1"],
                "figmog_at",
                json!({"x": 1.5, "y": 2.0, "page": "0:1", "limit": crate::query::DEFAULT_AT_LIMIT}),
            ),
            (&["spec", "1:1"], "figmog_spec", json!({"id": "1:1"})),
            (
                &["spec", "1:1", "--tailwind"],
                "figmog_spec",
                json!({"id": "1:1", "tailwind": true}),
            ),
            (
                &["spec", "1:1", "--no-tailwind"],
                "figmog_spec",
                json!({"id": "1:1", "tailwind": false}),
            ),
        ];
        for (argv, tool, want) in cases {
            let cli =
                Cli::try_parse_from(std::iter::once("figmog").chain(argv.iter().copied())).unwrap();
            let read = read_command(&cli.cmd).unwrap();
            let (name, args) = read.to_tool_call();
            assert_eq!((name, &args), (*tool, want), "{argv:?}");
            assert_eq!(
                ReadCommand::from_tool_args(name, &args),
                Ok(Some(read)),
                "{argv:?}"
            );
        }
        for argv in [
            &["pull"][..],
            &["call", "figmog_status"],
            &["images", "1:1"],
            &["assets", "export", "--icons", "1:1"],
        ] {
            let cli =
                Cli::try_parse_from(std::iter::once("figmog").chain(argv.iter().copied())).unwrap();
            assert_eq!(read_command(&cli.cmd), None, "{argv:?}");
        }
    }
}
