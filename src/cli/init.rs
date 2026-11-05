//! `figmog init` (NWS-2946): one non-interactive command from a React +
//! Tailwind repository to a working mirror and MCP registration.
//!
//! Steps, in order — each one idempotent, so a rerun converges on the same
//! files and a failed step is fixed by rerunning:
//!
//! 1. validate every flag (and read `--token-stdin`) before touching
//!    anything;
//! 2. write `figmog.toml` — created whole, or, when it exists, only the
//!    known tables it lacks are appended: an existing table is never
//!    edited, and a flag contradicting it is an error;
//! 3. save the token (`--token-stdin`, through [`crate::auth::login`]);
//! 4. keep `.figmog/` and `figmog.local.toml` out of Git
//!    ([`super::gitignore`]);
//! 5. move old `.figmog/<key>/db` mirrors to the shared mirror root
//!    ([`super::migrate`]);
//! 6. register the MCP server for Claude Code in the project's `.mcp.json`;
//! 7. add (or refresh) the marker-delimited figmog block in `AGENTS.md`;
//! 8. run the first pull (`--from-file` for offline use, `--no-pull` to
//!    skip) — last, so a failing pull leaves everything else set up.
//!
//! The project root is the discovered one, except that with no
//! `figmog.toml` up the tree, a working directory with its own
//! `package.json` (a monorepo package) becomes the root. The first pull
//! targets the merged default (a `figmog.local.toml` override included),
//! the file every later read uses; what init commits or reports as the
//! stack (`AGENTS.md`) comes from `figmog.toml` alone.
//!
//! The stack is detected only when `figmog.toml` has no `[stack]` table:
//! React from a `react` dependency in `package.json`, Tailwind from a
//! `tailwind.config.*` file or a `tailwindcss` / `@tailwindcss/vite` /
//! `@tailwindcss/postcss` dependency. `--framework` / `--styling` set them
//! without detection.

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use crate::ident::parse_file_ref;
use crate::project::{
    self, CONFIG_FILE, Framework, Layout, ProjectConfig, SETTINGS_TABLES, Settings, Styling,
};

/// `figmog init` flags: everything it would otherwise detect or default.
#[derive(Args)]
pub(super) struct InitArgs {
    /// The product file: Figma file key or URL (default: figmog.toml's
    /// `[files] default`, else the file last pulled here).
    #[arg(long)]
    file: Option<String>,
    /// The optional library file holding tokens and components (key or URL).
    #[arg(long)]
    library: Option<String>,
    /// Set the framework instead of detecting it (v1: react).
    #[arg(long)]
    framework: Option<String>,
    /// Set the styling instead of detecting it (v1: tailwind).
    #[arg(long)]
    styling: Option<String>,
    /// Token file path, relative to the project root
    /// (default: src/figmog-tokens.css).
    #[arg(long)]
    tokens: Option<String>,
    /// The local dev server's URL for the verify loop
    /// (default: http://localhost:5173).
    #[arg(long)]
    dev_server: Option<String>,
    /// Read a Figma personal access token from stdin and save it, as
    /// `figmog auth login --token-stdin` does.
    #[arg(long)]
    token_stdin: bool,
    /// Run the first pull from a saved GET /v1/files/:key response instead
    /// of the network.
    #[arg(long, conflicts_with = "no_pull")]
    from_file: Option<PathBuf>,
    /// Skip the first pull.
    #[arg(long)]
    no_pull: bool,
    /// The command `.mcp.json` runs to start the server (default: `figmog`,
    /// found on PATH). Replaces an existing `figmog` entry.
    #[arg(long)]
    mcp_command: Option<String>,
}

/// Header of a `figmog.toml` that `init` creates.
const CONFIG_HEADER: &str = "# figmog project settings (committed). Machine-specific overrides go in\n\
# figmog.local.toml, which is kept out of Git.\n";

/// The project's MCP config file (Claude Code's project scope).
const MCP_FILE: &str = ".mcp.json";
/// The server name registered in it.
const MCP_SERVER: &str = "figmog";
/// The agent instructions file and the markers around figmog's block.
const AGENTS_FILE: &str = "AGENTS.md";
const AGENTS_BEGIN: &str =
    "<!-- figmog:begin (managed by `figmog init`; edits inside are replaced) -->";
const AGENTS_END: &str = "<!-- figmog:end -->";

pub(super) fn cmd_init(args: InitArgs) -> Result<Value, String> {
    // 1. Flags first: nothing is written for an invalid one.
    let file_ref = |flag: &str, value: &Option<String>| {
        value
            .as_deref()
            .map(|v| {
                parse_file_ref(v).ok_or_else(|| format!("{flag}: not a Figma file key or URL: {v}"))
            })
            .transpose()
    };
    let file = file_ref("--file", &args.file)?;
    let library = file_ref("--library", &args.library)?;
    let framework = args
        .framework
        .as_deref()
        .map(Framework::parse)
        .transpose()
        .map_err(|e| format!("--framework: {e}"))?;
    let styling = args
        .styling
        .as_deref()
        .map(Styling::parse)
        .transpose()
        .map_err(|e| format!("--styling: {e}"))?;
    if let Some(tokens) = &args.tokens {
        project::check_project_path(tokens).map_err(|e| format!("--tokens: {e}"))?;
    }
    if let Some(url) = &args.dev_server {
        project::check_dev_server(url).map_err(|e| format!("--dev-server: {e}"))?;
    }
    if args
        .mcp_command
        .as_deref()
        .is_some_and(|c| c.trim().is_empty())
    {
        return Err("--mcp-command: must not be empty".into());
    }
    let token = if args.token_stdin {
        let token = super::auth::read_token_stdin()?;
        crate::auth::normalize_token(&token)?;
        Some(token)
    } else {
        None
    };

    let mut layout = Layout::discover_cwd()?;
    // A package in a monorepo (`apps/web`): with no figmog.toml anywhere up
    // the tree, the directory init runs in becomes the project root when it
    // has its own package.json — consistent with discovery, which walks up
    // from there and finds the new figmog.toml first.
    let cwd = std::env::current_dir().map_err(|e| format!("reading working directory: {e}"))?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    if !layout.config_path().is_file()
        && layout.project_root != cwd
        && cwd.join("package.json").is_file()
    {
        layout = layout.with_project_root(cwd);
    }
    let root = layout.project_root.clone();

    // 2. figmog.toml.
    let config_path = layout.config_path();
    let (existing_text, existing) = match std::fs::read_to_string(&config_path) {
        Ok(text) => {
            let config = project::parse_config(&text, CONFIG_FILE)?;
            (Some(text), Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(e) => return Err(format!("reading {}: {e}", config_path.display())),
    };
    let has = |table: &str| existing.as_ref().is_some_and(|c| c.has_table(table));
    let committed = existing.clone().unwrap_or_default();

    let (product, library) = if has("files") {
        let Some(product) = committed.default_key().map(str::to_string) else {
            return Err(format!(
                "{} has a [files] table without `default`; add `default = \"<file key or URL>\"` \
                 to it and rerun",
                config_path.display()
            ));
        };
        conflict(
            file.as_deref(),
            Some(&product),
            "files.default",
            &config_path,
        )?;
        if library.is_some() {
            conflict(
                library.as_deref(),
                committed.library_key(),
                "files.library",
                &config_path,
            )?;
        }
        (product, committed.library_key().map(str::to_string))
    } else {
        let product = match file {
            Some(file) => file,
            None => layout.default_key()?.ok_or_else(|| {
                "no product file: pass --file <Figma file URL or key>".to_string()
            })?,
        };
        (product, library)
    };

    let mut settings = committed.settings();
    let stack_source;
    if has("stack") {
        let stack = &settings.stack;
        conflict(
            framework.map(Framework::as_str),
            Some(stack.framework.as_str()),
            "stack.framework",
            &config_path,
        )?;
        conflict(
            styling.map(Styling::as_str),
            Some(stack.styling.as_str()),
            "stack.styling",
            &config_path,
        )?;
        conflict(
            args.tokens.as_deref(),
            Some(&stack.tokens),
            "stack.tokens",
            &config_path,
        )?;
        stack_source = json!({"framework": CONFIG_FILE, "styling": CONFIG_FILE});
    } else {
        let detected = detect_stack(&root, framework, styling)?;
        settings.stack.framework = detected.framework.0;
        settings.stack.styling = detected.styling.0;
        if let Some(tokens) = &args.tokens {
            settings.stack.tokens = tokens.clone();
        }
        stack_source = json!({"framework": detected.framework.1, "styling": detected.styling.1});
    }
    if has("verify") {
        conflict(
            args.dev_server.as_deref(),
            Some(&settings.verify.dev_server),
            "verify.dev_server",
            &config_path,
        )?;
    } else if let Some(url) = &args.dev_server {
        settings.verify.dev_server = url.clone();
    }

    let mut added_tables = Vec::new();
    let mut addition = String::new();
    if !has("files") {
        added_tables.push("files");
        addition.push_str(&project::render_files_table(&product, library.as_deref()));
    }
    for table in SETTINGS_TABLES {
        if !has(table) {
            added_tables.push(table);
            if !addition.is_empty() {
                addition.push('\n');
            }
            addition.push_str(&settings.render_table(table));
        }
    }
    let config_status = match &existing_text {
        None => {
            write(&config_path, &format!("{CONFIG_HEADER}\n{addition}"))?;
            "created"
        }
        Some(_) if addition.is_empty() => "unchanged",
        Some(text) => {
            let mut text = text.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&addition);
            write(&config_path, &text)?;
            "updated"
        }
    };
    // What was written must read back (and from here on, the layout is the
    // one every later command discovers, figmog.local.toml included).
    let layout = Layout::discover(&root)?;
    // What init commits (AGENTS.md, the reported stack) comes from
    // figmog.toml alone: machine-local overrides stay local.
    let committed_text = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("reading {}: {e}", config_path.display()))?;
    let settings = project::parse_config(&committed_text, CONFIG_FILE)?.settings();
    // The pull goes where every later read looks: the merged default.
    let pull_key = layout.default_key()?.unwrap_or_else(|| product.clone());

    // 3. The token.
    let token = match token {
        Some(token) => {
            let saved = crate::auth::login(&token)?;
            json!({"saved": true, "environment_override": saved["environment_override"]})
        }
        None => match crate::auth::status() {
            Ok(status) => json!({
                "saved": false,
                "configured": status["configured"],
                "source": status["source"],
            }),
            Err(error) => json!({"saved": false, "configured": false, "error": error}),
        },
    };

    // 4. .gitignore.
    let ignored = super::gitignore::ignore_local_mirror(&root, true)?;

    // 5. Old mirrors to the shared root.
    let migrated = super::migrate::cmd_migrate(&layout, None)?["migrated"].take();

    // 6. MCP registration; 7. AGENTS.md.
    let mcp = register_mcp(&root, args.mcp_command.as_deref())?;
    let agents = update_agents(&root, &settings)?;

    // 8. The first pull.
    let pull = if args.no_pull {
        json!({"skipped": true})
    } else {
        let churn =
            super::pull::pull_for_init(&layout, &pull_key, args.from_file).map_err(|e| {
                format!(
                    "first pull failed: {e} — everything else is set up; fix that and rerun \
                 `figmog init` (or run `figmog pull`)"
                )
            })?;
        json!({"skipped": false, "file": pull_key, "churn": churn})
    };

    Ok(json!({
        "project_root": root.display().to_string(),
        "config": {
            "path": config_path.display().to_string(),
            "status": config_status,
            "added_tables": added_tables,
        },
        "files": {"default": product, "library": library},
        "stack": {
            "framework": settings.stack.framework.as_str(),
            "styling": settings.stack.styling.as_str(),
            "tokens": settings.stack.tokens,
            "source": stack_source,
        },
        "token": token,
        "gitignore": {"updated": ignored},
        "migrated": migrated,
        "mcp": mcp,
        "agents": agents,
        "pull": pull,
    }))
}

/// A flag must agree with a table `figmog.toml` already has: `init` never
/// edits an existing table.
fn conflict(
    flag: Option<&str>,
    current: Option<&str>,
    key: &str,
    path: &Path,
) -> Result<(), String> {
    match flag {
        Some(flag) if Some(flag) != current => Err(format!(
            "{} already sets {key} to {} (a default when the key is absent); figmog init never \
             changes an existing table — edit the file instead",
            path.display(),
            current.map_or_else(|| "nothing".to_string(), |c| format!("{c:?}")),
        )),
        _ => Ok(()),
    }
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The detected (or flagged) stack, each with where it came from.
struct Stack {
    framework: (Framework, String),
    styling: (Styling, String),
}

/// `package.json`'s dependency sections, in the order they are searched.
const DEPENDENCY_SECTIONS: [&str; 3] = ["dependencies", "devDependencies", "peerDependencies"];
/// Packages that mean Tailwind, in the order they are searched.
const TAILWIND_PACKAGES: [&str; 3] = ["tailwindcss", "@tailwindcss/vite", "@tailwindcss/postcss"];
/// Tailwind config file names, in the order they are searched.
const TAILWIND_CONFIGS: [&str; 6] = [
    "tailwind.config.js",
    "tailwind.config.cjs",
    "tailwind.config.mjs",
    "tailwind.config.ts",
    "tailwind.config.cts",
    "tailwind.config.mts",
];

/// Detect React and Tailwind in `root` (flags win without looking). Both
/// must be found or flagged: v1 supports nothing else.
fn detect_stack(
    root: &Path,
    framework: Option<Framework>,
    styling: Option<Styling>,
) -> Result<Stack, String> {
    let package_path = root.join("package.json");
    let package: Option<Value> = match std::fs::read_to_string(&package_path) {
        Ok(text) => Some(
            serde_json::from_str(&text)
                .map_err(|e| format!("parsing {}: {e}", package_path.display()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading {}: {e}", package_path.display())),
    };
    let dependency = |name: &str| {
        let package = package.as_ref()?;
        DEPENDENCY_SECTIONS
            .iter()
            .find(|section| package[**section].get(name).is_some())
            .map(|section| format!("package.json {section}.{name}"))
    };
    let framework = match framework {
        Some(framework) => Some((framework, "--framework".to_string())),
        None => dependency("react").map(|source| (Framework::React, source)),
    };
    let styling = match styling {
        Some(styling) => Some((styling, "--styling".to_string())),
        None => TAILWIND_PACKAGES
            .iter()
            .find_map(|name| dependency(name))
            .or_else(|| {
                TAILWIND_CONFIGS
                    .iter()
                    .find(|name| root.join(name).is_file())
                    .map(|name| name.to_string())
            })
            .map(|source| (Styling::Tailwind, source)),
    };
    let mut missing = Vec::new();
    if framework.is_none() {
        missing.push(format!(
            "React (no `react` dependency in {}) — pass --framework react to set it anyway",
            package_path.display()
        ));
    }
    if styling.is_none() {
        missing.push(format!(
            "Tailwind (no tailwind.config.* in {} and no tailwindcss, @tailwindcss/vite or \
             @tailwindcss/postcss dependency) — pass --styling tailwind to set it anyway",
            root.display()
        ));
    }
    match (framework, styling) {
        (Some(framework), Some(styling)) => Ok(Stack { framework, styling }),
        _ => Err(format!(
            "could not detect {}; figmog v1 supports React + Tailwind",
            missing.join("; ")
        )),
    }
}

/// Register figmog in the project's `.mcp.json` (Claude Code's project
/// scope): add the `figmog` server, keep an existing entry unless
/// `--mcp-command` asks to replace it, and leave every other server as it
/// was. The file is rewritten (sorted keys, two-space indent) only when the
/// entry changes.
fn register_mcp(root: &Path, command: Option<&str>) -> Result<Value, String> {
    let path = root.join(MCP_FILE);
    let desired = json!({
        "type": "stdio",
        "command": command.unwrap_or("figmog"),
        "args": ["serve"],
        "env": {},
    });
    let mut doc: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("parsing {}: {e} — fix it and rerun", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let not_object = || {
        format!(
            "{} is not a JSON object with an `mcpServers` object",
            path.display()
        )
    };
    let servers = doc
        .as_object_mut()
        .ok_or_else(not_object)?
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(not_object)?;
    let status = match servers.get(MCP_SERVER) {
        Some(existing) if *existing == desired => "unchanged",
        Some(_) if command.is_none() => "kept",
        Some(_) => "updated",
        None => "added",
    };
    if matches!(status, "updated" | "added") {
        servers.insert(MCP_SERVER.to_string(), desired);
    }
    let server = servers[MCP_SERVER].clone();
    if matches!(status, "updated" | "added") {
        let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        write(&path, &format!("{text}\n"))?;
    }
    Ok(json!({
        "path": path.display().to_string(),
        "status": status,
        "server": server,
    }))
}

/// figmog's block in `AGENTS.md`, markers included, ending in a newline.
fn agents_block(settings: &Settings) -> String {
    let viewports = project::VERIFY_VIEWPORTS
        .iter()
        .map(|w| format!("{w} px"))
        .collect::<Vec<_>>()
        .join(" and ");
    format!(
        "{AGENTS_BEGIN}\n\
         ## Figma designs (figmog)\n\
         \n\
         This repository mirrors its Figma files locally with figmog. Project settings are in \
         `figmog.toml` (committed); machine-specific overrides go in `figmog.local.toml`.\n\
         \n\
         - Read the design through the `figmog` MCP server (registered in `.mcp.json`): its \
         `figmog_*` tools answer from the local mirror, with no Figma rate limits. Start with \
         `figmog_status`, then `figmog_search`, `figmog_tree` and `figmog_node`.\n\
         - The same reads work in a shell: `figmog status`, `figmog search <text>`, \
         `figmog tree`, `figmog get <node-id>`, `figmog spec <node-id>`.\n\
         - `figmog pull` refreshes the mirror; `figmog doctor` checks the setup; \
         `figmog settings` prints the settings below.\n\
         - Stack: {framework} + {styling}. Design tokens live in `{tokens}` (written by \
         `figmog tokens export --out {tokens}`); use them instead of hard-coded values.\n\
         - Verify against the design at {viewports} wide, {scheme} mode, {state} state.\n\
         - Never commit `.figmog/` or `figmog.local.toml`.\n\
         {AGENTS_END}\n",
        framework = match settings.stack.framework {
            Framework::React => "React",
        },
        styling = match settings.stack.styling {
            Styling::Tailwind => "Tailwind CSS",
        },
        tokens = settings.stack.tokens,
        scheme = project::VERIFY_COLOR_SCHEME,
        state = project::VERIFY_STATE,
    )
}

/// Create `AGENTS.md` with figmog's block, replace the block between its
/// markers, or append it — never touching anything outside the markers.
fn update_agents(root: &Path, settings: &Settings) -> Result<Value, String> {
    let path = root.join(AGENTS_FILE);
    let block = agents_block(settings);
    let (text, status) = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (block, "created"),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
        Ok(text) => {
            let begins: Vec<usize> = text.match_indices(AGENTS_BEGIN).map(|(i, _)| i).collect();
            let ends: Vec<usize> = text.match_indices(AGENTS_END).map(|(i, _)| i).collect();
            match (begins.as_slice(), ends.as_slice()) {
                ([], []) => {
                    let mut out = text.clone();
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&block);
                    (out, "appended")
                }
                ([begin], [end]) if begin < end => {
                    let after = end + AGENTS_END.len();
                    let out = format!(
                        "{}{}{}",
                        &text[..*begin],
                        block.trim_end_matches('\n'),
                        &text[after..]
                    );
                    let status = if out == text { "unchanged" } else { "updated" };
                    (out, status)
                }
                _ => {
                    return Err(format!(
                        "{} has unbalanced figmog markers (`{AGENTS_BEGIN}` … `{AGENTS_END}`); \
                         fix or remove them and rerun",
                        path.display()
                    ));
                }
            }
        }
    };
    if status != "unchanged" {
        write(&path, &text)?;
    }
    Ok(json!({"path": path.display().to_string(), "status": status}))
}

/// `figmog settings`: the resolved settings of the discovered project.
pub(super) fn cmd_settings(layout: &Layout) -> Value {
    let config: &ProjectConfig = &layout.config;
    let mut out = json!({
        "project_root": layout.project_root.display().to_string(),
        "config": layout.config_path().is_file().then(|| layout.config_path().display().to_string()),
        "files": config.files,
        "unknown_keys": config.unknown_keys(),
    });
    let settings = layout.settings();
    if let (Some(out), Value::Object(json)) = (out.as_object_mut(), settings.to_json()) {
        out.extend(json);
    }
    // The token file's absolute path, for tools outside this binary.
    out["stack"]["tokens_path"] = json!(
        settings
            .stack
            .tokens_path(&layout.project_root)
            .display()
            .to_string()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn agents_block_replaces_only_its_own_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AGENTS_FILE);
        assert_eq!(
            update_agents(dir.path(), &settings()).unwrap()["status"],
            "created"
        );
        let created = std::fs::read_to_string(&path).unwrap();
        assert!(created.starts_with(AGENTS_BEGIN) && created.ends_with(&format!("{AGENTS_END}\n")));
        assert_eq!(
            update_agents(dir.path(), &settings()).unwrap()["status"],
            "unchanged"
        );

        std::fs::write(&path, "# Rules\nkeep me").unwrap();
        assert_eq!(
            update_agents(dir.path(), &settings()).unwrap()["status"],
            "appended"
        );
        let appended = std::fs::read_to_string(&path).unwrap();
        assert!(
            appended.starts_with("# Rules\nkeep me\n\n<!-- figmog:begin"),
            "{appended}"
        );

        // A stale block between other content is replaced in place.
        std::fs::write(
            &path,
            format!("top\n{AGENTS_BEGIN}\nold\n{AGENTS_END}\nbottom\n"),
        )
        .unwrap();
        assert_eq!(
            update_agents(dir.path(), &settings()).unwrap()["status"],
            "updated"
        );
        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with("top\n<!-- figmog:begin"), "{updated}");
        assert!(
            updated.ends_with(&format!("{AGENTS_END}\nbottom\n")),
            "{updated}"
        );
        assert!(!updated.contains("\nold\n"));
        assert_eq!(
            update_agents(dir.path(), &settings()).unwrap()["status"],
            "unchanged"
        );

        std::fs::write(&path, format!("{AGENTS_END}\n{AGENTS_BEGIN}\n")).unwrap();
        assert!(
            update_agents(dir.path(), &settings())
                .unwrap_err()
                .contains("unbalanced")
        );
    }

    #[test]
    fn mcp_registration_merges_and_keeps_other_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        std::fs::write(
            &path,
            r#"{"mcpServers": {"other": {"command": "x"}}, "z": 1}"#,
        )
        .unwrap();
        assert_eq!(register_mcp(dir.path(), None).unwrap()["status"], "added");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["other"], json!({"command": "x"}));
        assert_eq!(doc["z"], 1);
        assert_eq!(
            doc["mcpServers"]["figmog"],
            json!({"type": "stdio", "command": "figmog", "args": ["serve"], "env": {}})
        );
        assert_eq!(
            register_mcp(dir.path(), None).unwrap()["status"],
            "unchanged"
        );
        // A customized entry is kept, unless --mcp-command replaces it.
        assert_eq!(
            register_mcp(dir.path(), Some("/opt/figmog")).unwrap()["status"],
            "updated"
        );
        assert_eq!(register_mcp(dir.path(), None).unwrap()["status"], "kept");
        std::fs::write(&path, "[]").unwrap();
        assert!(register_mcp(dir.path(), None).is_err());
        std::fs::write(&path, "{").unwrap();
        assert!(
            register_mcp(dir.path(), None)
                .unwrap_err()
                .contains("parsing")
        );
    }

    #[test]
    fn detection_reads_dependencies_and_tailwind_configs() {
        let dir = tempfile::tempdir().unwrap();
        let error = detect_stack(dir.path(), None, None).err().unwrap();
        assert!(
            error.contains("--framework react") && error.contains("--styling tailwind"),
            "{error}"
        );
        let stack = detect_stack(dir.path(), Some(Framework::React), Some(Styling::Tailwind))
            .ok()
            .unwrap();
        assert_eq!(stack.framework.1, "--framework");

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies": {"react": "19.0.0"}, "devDependencies": {"@tailwindcss/vite": "4.0.0"}}"#,
        )
        .unwrap();
        let stack = detect_stack(dir.path(), None, None).ok().unwrap();
        assert_eq!(stack.framework.1, "package.json dependencies.react");
        assert_eq!(
            stack.styling.1,
            "package.json devDependencies.@tailwindcss/vite"
        );

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"peerDependencies": {"react": "*"}}"#,
        )
        .unwrap();
        let error = detect_stack(dir.path(), None, None).err().unwrap();
        assert!(
            error.contains("Tailwind") && !error.contains("React ("),
            "{error}"
        );
        std::fs::write(dir.path().join("tailwind.config.ts"), "export default {}").unwrap();
        let stack = detect_stack(dir.path(), None, None).ok().unwrap();
        assert_eq!(stack.styling.1, "tailwind.config.ts");

        std::fs::write(dir.path().join("package.json"), "{").unwrap();
        assert!(
            detect_stack(dir.path(), None, None)
                .err()
                .unwrap()
                .contains("parsing")
        );
    }
}
