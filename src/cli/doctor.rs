//! `figmog doctor` (NWS-2946): check the setup `figmog init` makes.
//!
//! Every check reports `ok`, `warn` or `fail` with a one-line detail, in a
//! fixed order. Network checks are bounded (a few seconds each) and an
//! unreachable network is a `warn` ("could not check"), never a hang or a
//! `fail`; a check that cannot run because an earlier one failed is a
//! `warn` saying so. The report is printed either way; the command exits 1
//! when any check failed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::project::Layout;
use crate::upstream::{HttpUpstream, UpstreamMcp};

/// Budget for each Figma REST check.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for each connection attempt to the dev server.
const DEV_SERVER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

/// Run every check from `cwd`'s project; `upstream` is the desktop MCP URL.
/// Returns the report and whether any check failed.
pub(super) fn cmd_doctor(upstream: &str) -> (Value, bool) {
    let mut checks = Vec::new();

    // config
    let layout = Layout::discover_cwd();
    checks.push(match &layout {
        Err(error) => check("config", Status::Fail, error.clone()),
        Ok(layout) if !layout.config_path().is_file() => check(
            "config",
            Status::Warn,
            format!(
                "no figmog.toml in {} — defaults apply; run `figmog init`",
                layout.project_root.display()
            ),
        ),
        Ok(layout) if !layout.config.unknown_keys().is_empty() => check(
            "config",
            Status::Warn,
            format!(
                "{}: unknown settings ignored: {}",
                layout.config_path().display(),
                layout
                    .config
                    .unknown_keys()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Ok(layout) => check(
            "config",
            Status::Ok,
            layout.config_path().display().to_string(),
        ),
    });
    let layout = layout.ok();

    // token
    let token = match crate::auth::status() {
        Err(error) => {
            checks.push(check("token", Status::Fail, error));
            None
        }
        Ok(status) => match crate::auth::token() {
            Ok(Some(token)) => {
                checks.push(check(
                    "token",
                    Status::Ok,
                    format!(
                        "configured ({})",
                        status["source"].as_str().unwrap_or("unknown")
                    ),
                ));
                Some(token)
            }
            Ok(None) => {
                checks.push(check(
                    "token",
                    Status::Fail,
                    crate::auth::missing_token("file access"),
                ));
                None
            }
            Err(error) => {
                checks.push(check("token", Status::Fail, error));
                None
            }
        },
    };

    // mirror
    let key = match &layout {
        None => {
            checks.push(check("mirror", Status::Warn, "skipped: no valid config"));
            None
        }
        Some(layout) => match layout.default_key() {
            Err(error) => {
                checks.push(check("mirror", Status::Fail, error));
                None
            }
            Ok(None) => {
                checks.push(check(
                    "mirror",
                    Status::Warn,
                    "no product file: set `[files] default` in figmog.toml (`figmog init --file …`)",
                ));
                None
            }
            Ok(Some(key)) => {
                checks.push(mirror_check(layout, &key));
                Some(key)
            }
        },
    };

    // file_access, variables_access
    match (&token, &key) {
        (Some(token), Some(key)) => {
            let api = UreqApi::for_sessions(token.clone()).with_timeout(API_TIMEOUT);
            let access = file_access_check(&api, key);
            let reached = access.status == Status::Ok;
            checks.push(access);
            checks.push(if reached {
                variables_check(&api, key)
            } else {
                check(
                    "variables_access",
                    Status::Warn,
                    "skipped: file access not confirmed",
                )
            });
        }
        _ => {
            let why = if token.is_none() {
                "skipped: no token"
            } else {
                "skipped: no product file"
            };
            checks.push(check("file_access", Status::Warn, why));
            checks.push(check("variables_access", Status::Warn, why));
        }
    }

    // desktop_mcp
    let mut desktop = HttpUpstream::new(upstream.to_string());
    checks.push(match desktop.initialize() {
        Ok(()) => check(
            "desktop_mcp",
            Status::Ok,
            format!("connected to {upstream} ({} tools)", desktop.tools().len()),
        ),
        Err(error) => check(
            "desktop_mcp",
            Status::Warn,
            format!(
                "Figma's desktop MCP server is not reachable at {upstream} ({error}); open the \
                 Figma desktop app and enable its Dev Mode MCP server — figmog's own tools work \
                 without it"
            ),
        ),
    });

    // mcp_config, dev_server
    match &layout {
        None => {
            checks.push(check(
                "mcp_config",
                Status::Warn,
                "skipped: no valid config",
            ));
            checks.push(check(
                "dev_server",
                Status::Warn,
                "skipped: no valid config",
            ));
        }
        Some(layout) => {
            checks.push(mcp_config_check(&layout.project_root));
            let verify = layout.settings().verify;
            checks.push(if verify.enabled {
                dev_server_check(&verify.dev_server)
            } else {
                check(
                    "dev_server",
                    Status::Ok,
                    "verify loop off ([verify] loop = false); not checked",
                )
            });
        }
    }

    let worst = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok);
    let report = json!({
        "status": worst.as_str(),
        "checks": checks
            .iter()
            .map(|c| json!({"name": c.name, "status": c.status.as_str(), "detail": c.detail}))
            .collect::<Vec<_>>(),
    });
    (report, worst == Status::Fail)
}

/// Where the product file's mirror is — without opening it (a running
/// serve may hold it).
fn mirror_check(layout: &Layout, key: &str) -> Check {
    match layout.active_store(key) {
        Err(error) => check("mirror", Status::Fail, error),
        Ok(store) if store.migration_pending => check(
            "mirror",
            Status::Warn,
            format!(
                "{key} is still at {}; run `figmog migrate` (or `figmog init`)",
                store.path.display()
            ),
        ),
        Ok(store) if !store.path.is_dir() => check(
            "mirror",
            Status::Warn,
            format!("{key} is not mirrored yet; run `figmog pull`"),
        ),
        Ok(store) => check(
            "mirror",
            Status::Ok,
            format!("{key} at {}", store.path.display()),
        ),
    }
}

/// A network-level failure: the check could not run, so it warns.
fn unreachable(name: &'static str, error: &ApiError) -> Check {
    check(
        name,
        Status::Warn,
        format!("could not reach Figma ({error}); rerun when online"),
    )
}

/// `GET /v1/files/:key/meta` (Tier 3, cheap).
fn file_access_check(api: &dyn FigmaApi, key: &str) -> Check {
    match api.file_meta(key) {
        Ok(meta) => check("file_access", Status::Ok, format!("{key}: {:?}", meta.name)),
        Err(error @ (ApiError::Network(_) | ApiError::RateLimited { .. })) => {
            unreachable("file_access", &error)
        }
        Err(ApiError::Auth) => check(
            "file_access",
            Status::Fail,
            format!(
                "the token cannot open {key} (403): check the token and that its account can \
                 view the file"
            ),
        ),
        Err(error) => check("file_access", Status::Fail, format!("{key}: {error}")),
    }
}

/// `GET /v1/files/:key/variables/local` (Enterprise plans only). Not
/// having it is a `warn`: variables can still be imported.
fn variables_check(api: &dyn FigmaApi, key: &str) -> Check {
    match api.variables_local(key) {
        Ok(Some(_)) => check(
            "variables_access",
            Status::Ok,
            "the Variables REST API is available; `figmog pull` syncs variables",
        ),
        Ok(None) => check(
            "variables_access",
            Status::Warn,
            "this plan has no Variables REST API (Enterprise only): import variables with \
             `figmog import-variables <export.json>`",
        ),
        Err(error @ (ApiError::Network(_) | ApiError::RateLimited { .. })) => {
            unreachable("variables_access", &error)
        }
        Err(error) => check("variables_access", Status::Warn, error.to_string()),
    }
}

/// figmog registered in the project's `.mcp.json`, with a command that
/// resolves.
fn mcp_config_check(root: &Path) -> Check {
    let path = root.join(".mcp.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return check(
                "mcp_config",
                Status::Warn,
                format!(
                    "no {} — run `figmog init` to register figmog for Claude Code",
                    path.display()
                ),
            );
        }
        Err(e) => {
            return check(
                "mcp_config",
                Status::Fail,
                format!("reading {}: {e}", path.display()),
            );
        }
    };
    let doc: Value = match serde_json::from_str(&text) {
        Ok(doc) => doc,
        Err(e) => {
            return check(
                "mcp_config",
                Status::Fail,
                format!("parsing {}: {e}", path.display()),
            );
        }
    };
    let server = &doc["mcpServers"]["figmog"];
    let Some(command) = server["command"].as_str() else {
        return check(
            "mcp_config",
            Status::Warn,
            format!(
                "{} has no `figmog` server — run `figmog init`",
                path.display()
            ),
        );
    };
    if !command_resolves(command, root) {
        return check(
            "mcp_config",
            Status::Warn,
            format!(
                "{}: command {command:?} is not an executable or on PATH",
                path.display()
            ),
        );
    }
    check(
        "mcp_config",
        Status::Ok,
        format!("{}: {command}", path.display()),
    )
}

/// An executable path (relative to `root`), or a bare name found on PATH.
fn command_resolves(command: &str, root: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return executable(&root.join(command));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| executable(&dir.join(command)))
    })
}

/// A TCP connection to the dev server's host and port (bounded: name
/// resolution included, see [`dev_server_addrs`]).
fn dev_server_check(url: &str) -> Check {
    let target = url::Url::parse(url).ok().and_then(|u| {
        let host = u.host()?.to_owned();
        Some((host, u.port_or_known_default()?))
    });
    let Some((host, port)) = target else {
        return check(
            "dev_server",
            Status::Fail,
            format!("{url} has no host and port"),
        );
    };
    let addrs = match dev_server_addrs(&host, port, DEV_SERVER_TIMEOUT) {
        Ok(addrs) => addrs,
        Err(error) => {
            return check(
                "dev_server",
                Status::Warn,
                format!("{url}: could not check ({error})"),
            );
        }
    };
    let reachable = addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, DEV_SERVER_TIMEOUT).is_ok());
    if reachable {
        check("dev_server", Status::Ok, format!("{url} is reachable"))
    } else {
        check(
            "dev_server",
            Status::Warn,
            format!("{url} is not reachable — start the dev server (or set [verify] dev_server)"),
        )
    }
}

/// The addresses to try for the dev server, never blocking past `deadline`:
/// an IP literal as is; `localhost` and `*.localhost` straight to loopback
/// (`127.0.0.1` and `::1`, as RFC 6761 reserves them) without asking a
/// resolver; any other name through the system resolver under `deadline`.
fn dev_server_addrs(
    host: &url::Host,
    port: u16,
    deadline: Duration,
) -> Result<Vec<SocketAddr>, String> {
    match host {
        url::Host::Ipv4(ip) => Ok(vec![SocketAddr::new(IpAddr::V4(*ip), port)]),
        url::Host::Ipv6(ip) => Ok(vec![SocketAddr::new(IpAddr::V6(*ip), port)]),
        url::Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            let domain = domain.trim_end_matches('.');
            if domain == "localhost" || domain.ends_with(".localhost") {
                return Ok(vec![
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
                ]);
            }
            let name = domain.to_string();
            resolve_within(deadline, move || {
                (name.as_str(), port)
                    .to_socket_addrs()
                    .map(|a| a.collect())
                    .map_err(|e| e.to_string())
            })
        }
    }
}

/// Run a blocking `resolve` on a detached thread and wait at most
/// `deadline` for it. The thread is never joined: a resolver stuck past the
/// deadline is left to finish (or not) on its own, and its late answer is
/// dropped.
fn resolve_within(
    deadline: Duration,
    resolve: impl FnOnce() -> Result<Vec<SocketAddr>, String> + Send + 'static,
) -> Result<Vec<SocketAddr>, String> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("figmog-doctor-resolve".into())
        .spawn(move || {
            let _ = tx.send(resolve());
        });
    if let Err(e) = spawned {
        return Err(format!("starting name resolution: {e}"));
    }
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "name resolution took longer than {} s",
            deadline.as_secs_f64()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("name resolution failed".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Review finding (NWS-2946): `localhost` and `*.localhost` go straight
    /// to loopback, never through a resolver that could stall doctor.
    #[test]
    fn localhost_names_resolve_to_loopback_without_dns() {
        let loopback = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5173),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5173),
        ];
        for name in ["localhost", "app.localhost", "App.LocalHost."] {
            let host = url::Host::Domain(name.to_string());
            // A zero deadline: any resolver call would time out.
            assert_eq!(
                dev_server_addrs(&host, 5173, Duration::ZERO).unwrap(),
                loopback,
                "{name}"
            );
        }
        let url = url::Url::parse("http://[::1]:5173").unwrap();
        assert_eq!(
            dev_server_addrs(&url.host().unwrap().to_owned(), 5173, Duration::ZERO).unwrap(),
            vec![loopback[1]]
        );
    }

    /// Review finding (NWS-2946): a resolver stuck past the deadline is
    /// abandoned (its thread never joined), reported as a timeout.
    #[test]
    fn slow_resolution_is_abandoned_at_the_deadline() {
        let (release, wait) = mpsc::channel::<()>();
        let started = Instant::now();
        let error = resolve_within(Duration::from_millis(100), move || {
            let _ = wait.recv();
            Ok(Vec::new())
        })
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.contains("took longer"), "{error}");
        drop(release);

        let fast = resolve_within(Duration::from_secs(5), || {
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1)])
        })
        .unwrap();
        assert_eq!(fast.len(), 1);
    }
}
