//! `figmog pull`: fetch-flatten-sync-evict, the typed [`PullError`] it can
//! fail with, and `.figmog/current` (the "which file did we last touch"
//! pointer every other command's [`super::resolve_db`] reads).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids, sync};
use crate::watch::BACKOFF_CAP;

use super::{Db, write_json};

/// Errors from [`do_pull`]: either a typed API failure (so callers can act
/// on rate limits) or any other pull-mechanics failure. `Display` matches
/// the plain-string messages `do_pull` used to produce, so `cmd_pull`'s
/// user-facing errors are unchanged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PullError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("{0}")]
    Other(String),
}

impl From<String> for PullError {
    fn from(s: String) -> Self {
        PullError::Other(s)
    }
}

pub(super) fn cmd_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
    geometry: bool,
) -> Result<(), String> {
    let (churn, _name, _version) = do_pull(db, file, from_file, fresh, geometry).map_err(|e| {
        let message = e.to_string();
        // A reachable serve already took this pull (NWS-2963,
        // `cmd_pull_routed`); this direct path meets a held store only via
        // `--db`/`--no-socket` or an owner without a socket. A serve owner
        // is named by the lock error itself; for an unnamed one, point at
        // the escape hatch that reaches a serve (`figmog call
        // figmog_sync`) rather than leaving the generic message alone.
        // Scoped to exactly this message (not every `do_pull` failure) so
        // an unrelated error (bad JSON, a real network failure, ...) isn't
        // given a hint that doesn't apply to it.
        if message == super::STORE_LOCKED_MSG {
            format!("{message} — or ask the running serve: figmog call figmog_sync")
        } else {
            message
        }
    })?;
    finish_pull(
        db,
        &serde_json::to_value(&churn).map_err(|e| e.to_string())?,
    )
}

/// `figmog pull` through the running serve that owns the store (NWS-2963):
/// a saved response is read and parsed here, in the invoking worktree, with
/// the direct path's errors, and sent as content; serve validates it before
/// touching the store, applies it on the file's worker (`--fresh` behind
/// the per-store reset barrier) and answers once the pull committed and was
/// published. The output is the direct path's.
pub(super) fn cmd_pull_routed(
    db: &Db,
    key: &str,
    stream: std::os::unix::net::UnixStream,
    from_file: Option<PathBuf>,
    fresh: bool,
    geometry: bool,
) -> Result<(), String> {
    let mut args = serde_json::json!({"file": key, "fresh": fresh, "geometry": geometry});
    if let Some(path) = from_file {
        args["from_file"] = read_saved_response(&path)?;
    }
    let done = super::socket::write_over(stream, crate::serve::writes::PULL, args)?;
    if let Some(layout) = &db.layout {
        write_current(&layout.local_root, key)?;
    }
    finish_pull(db, &done["result"])
}

/// After a successful pull (either path): keep `.figmog/` ignored, then
/// print the churn.
fn finish_pull(db: &Db, churn: &Value) -> Result<(), String> {
    if db.key.is_some()
        && let Some(layout) = &db.layout
    {
        let ignored = super::gitignore::ignore_local_mirror(
            &layout.project_root,
            layout
                .project_root
                .join(crate::project::CONFIG_FILE)
                .is_file(),
        );
        if let Err(error) = ignored {
            // Sync already succeeded. A read-only/invalid ignore file
            // must not turn that into a failed pull or corrupt stdout.
            eprintln!(
                "{}",
                serde_json::json!({"warning": format!("could not ignore .figmog/: {error}")})
            );
        }
    }
    write_json(churn)
}

/// Read and parse a saved `GET /v1/files/:key` response (`--from-file`).
fn read_saved_response(path: &std::path::Path) -> Result<Value, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// The pull mechanics without any printing. `.figmog/current` is written
/// only once the sync below has actually happened, so a failed pull never
/// repoints later commands at a nonexistent mirror.
///
/// `geometry` is this call's own override (spec §4 stickiness): the
/// network fetch requests `?geometry=paths` when `geometry` is set *or*
/// the mirror's already-stored config has it on
/// (`store::effective_geometry`); the resulting flag is persisted back via
/// `store::upsert_mirror_config` regardless of path, so it survives to
/// drive the next pull even for `--from-file` (which never touches the
/// network, and so never reads this flag for a fetch parameter at all —
/// see the `from_file` match arm below).
pub(super) fn do_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
    geometry: bool,
) -> Result<(Churn, String, String), PullError> {
    // `vars_resp` is only ever `Some` on the network path — `--from-file`
    // ingests a saved `GET /v1/files/:key` response and never touches the
    // network at all, so it never calls `variables_local` either.
    let (resp, vars_resp): (Value, Option<Value>) = match from_file {
        Some(path) => (read_saved_response(&path)?, None),
        None => {
            let key = db
                .key
                .clone()
                .or_else(|| file.and_then(|f| parse_file_ref(&f)))
                .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
            let token =
                crate::auth::token()?.ok_or_else(|| crate::auth::missing_token("network pulls"))?;
            // Peek the mirror's stored geometry setting (spec §4) *before*
            // fetching, so a plain re-pull of a mirror that was ever
            // pulled with `--geometry` keeps asking for it. `--fresh`
            // ignores its stored config (the documented way to turn
            // geometry back off). A first pull (no store on disk yet)
            // never opens one here at all, so a token
            // failure above still leaves no store trace (unchanged from
            // pre-v0.0.2 — see `failed_pull_does_not_persist_current_or_
            // create_store`). When a store does exist, this is a second,
            // short-lived open dropped before the real one further down —
            // fjall allows only one open handle per store per process at a
            // time, not one ever; dropping releases its lock.
            let stored_geometry = if fresh || !db.path.exists() {
                false
            } else {
                db.open(|path| crate::open_store!(path))
                    .map(|st| {
                        st.rtx(|(.., mirror_config, _, _, _)| store::read_geometry(&mirror_config))
                    })
                    .unwrap_or(false)
            };
            let request_geometry = store::effective_geometry(geometry, stored_geometry);
            let api = UreqApi::new(token);
            let resp = api.file(&key, request_geometry)?;
            // Opportunistic Enterprise variables sync (spec §12): `Ok(None)`
            // on non-Enterprise plans is not an error — v1 behavior
            // (import/inference, sweep-exempt) holds unchanged below.
            let vars_resp = api.variables_local(&key)?;
            (resp, vars_resp)
        }
    };

    apply_pull(db, &resp, vars_resp.as_ref(), fresh, geometry)
}

/// [`apply_pull`] for in-crate tests comparing serve's writes with the
/// direct path (NWS-2963).
#[cfg(test)]
pub(crate) fn apply_pull_at(
    path: &std::path::Path,
    resp: &Value,
    fresh: bool,
    geometry: bool,
) -> Result<Churn, String> {
    let db = Db {
        path: path.to_path_buf(),
        key: None,
        layout: None,
    };
    apply_pull(&db, resp, None, fresh, geometry)
        .map(|(churn, ..)| churn)
        .map_err(|e| e.to_string())
}

/// Validate both responses before opening or changing a mirror. Fresh
/// replacement preserves an in-store ownership anchor while clearing contents.
fn apply_pull(
    db: &Db,
    resp: &Value,
    vars_resp: Option<&Value>,
    fresh: bool,
    geometry: bool,
) -> Result<(Churn, String, String), PullError> {
    let mut flattened = flatten_file(resp).map_err(|e| e.to_string())?;
    if let Some(v) = vars_resp {
        flattened
            .recs
            .extend(crate::vars::enterprise_snapshot_recs(v).map_err(|e| e.to_string())?);
    }

    let mut st = db.open(|path| crate::open_store!(path))?;
    if fresh {
        st = st.reset(|path| crate::open_store!(path))?;
    }
    let mut prior: BTreeSet<crate::model::Id> =
        st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
            collect_sweepable(&nodes, &components, &component_sets, &styles)
        });
    if vars_resp.is_some() {
        let stored_var_ids = st.rtx(
            |(_, _, _, _, variables, variable_collections, _, _, _, _, _, _)| {
                collect_variable_ids(&variables, &variable_collections)
            },
        );
        prior.extend(stored_var_ids);
    }
    let prior_version =
        st.rtx(|(_, _, _, _, _, _, meta, _, _, _, _, _)| meta.get(&0).map(|m| m.version.clone()));
    let churn = sync(&mut st, &prior, &flattened, now_ms());

    // Every caller of `do_pull` (`pull` and `figmog call figmog_sync`) goes
    // through here, so eviction lives here rather than duplicated at each
    // call site (build design §12: a
    // version-changing pull sweeps stale `proxy_cache` rows; v0.0.2 spec §5
    // extends this to stale `images` rows the same way). `figmog serve`'s
    // own pull paths don't call `do_pull` — they keep their own inline
    // eviction blocks, since they already hold `st` open and re-opening it
    // here would hit the same single-open-per-process wall `figmog call
    // figmog_sync` used to.
    if prior_version.as_deref() != Some(flattened.file.version.as_str()) {
        let mut stale = st.rtx(|(_, _, _, _, _, _, _, cache, _, images, _, _)| {
            let mut stale = crate::store::stale_cache_ids(&cache, &flattened.file.version);
            stale.extend(crate::store::stale_image_ids(
                &images,
                &flattened.file.version,
            ));
            stale
        });
        if !stale.is_empty() {
            stale.sort();
            crate::store::evict_stale_cache(&mut st, &stale);
        }
    }

    // Persist the sticky geometry flag (spec §4): this call's own flag OR
    // whatever's already stored in *this* handle (re-read here rather than
    // reusing `stored_geometry` above — that value came from a separate,
    // now-dropped peek on the network path, and doesn't exist at all on
    // the `--from-file` path, which still must persist stickiness even
    // though it never requested anything over the network — see this
    // function's doc comment). `--fresh` already wiped any prior row, so
    // this store's own current read is `false` there, giving exactly
    // `geometry` as the persisted value, same as the fetch decision above.
    let stored_now = st.rtx(|(.., mirror_config, _, _, _)| store::read_geometry(&mirror_config));
    store::upsert_mirror_config(&mut st, store::effective_geometry(geometry, stored_now));

    if let (Some(key), Some(layout)) = (&db.key, &db.layout) {
        write_current(&layout.local_root, key)?;
    }

    // Move what this pull wrote out of fjall's journal (NWS-2941), so the
    // next cold open doesn't replay it. A zero-churn pull wrote nothing
    // worth it (just its meta/config rows).
    if fresh || churn.wrote() {
        st.close_and_compact();
    }

    Ok((
        churn,
        flattened.file.name.clone(),
        flattened.file.version.clone(),
    ))
}

/// How long a failed pull's caller (`figmog serve`'s sessions, via
/// `sessions.rs`) should wait before retrying, and advance the per-loop
/// backoff state. `RateLimited` honors `Retry-After` (never less than the
/// normal poll interval); anything else gets the same exponential backoff
/// discipline the [`Watcher`](crate::watch::Watcher) uses for Tier-3 meta
/// failures.
pub(crate) fn pull_failure_wait(
    err: &PullError,
    backoff: &mut Duration,
    interval: Duration,
) -> Duration {
    if let PullError::Api(ApiError::RateLimited { retry_after }) = err {
        interval.max(*retry_after)
    } else {
        let wait = *backoff;
        *backoff = (*backoff * 2).min(BACKOFF_CAP);
        wait
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Record `key` in `<local_root>/current` (the project's per-checkout
/// `.figmog`), the fallback default file when `figmog.toml` names none.
pub(crate) fn write_current(local_root: &std::path::Path, key: &str) -> Result<(), String> {
    let root = crate::private_dir::ensure(local_root)?;
    std::fs::write(root.join("current"), key).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::BACKOFF_START;

    #[test]
    fn invalid_variables_fail_before_fresh_opens_or_resets_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db {
            path: dir.path().join("db"),
            key: None,
            layout: None,
        };
        let file = serde_json::json!({
            "name": "Synthetic", "version": "1", "lastModified": "2026-01-01T00:00:00Z",
            "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT"}
        });
        let invalid_vars = serde_json::json!({"meta": {"variables": {}}});

        let error = apply_pull(&db, &file, Some(&invalid_vars), true, false).unwrap_err();
        assert!(error.to_string().contains("variableCollections"));
        assert!(!db.path.exists(), "invalid data must not create a store");

        apply_pull(&db, &file, None, false, true).unwrap();
        let st = db.open(|path| crate::open_store!(path)).unwrap();
        let meta = st.get(&crate::model::Id::Meta);
        let config = st.get(&crate::model::Id::MirrorConfig);
        // Holding the store also proves validation precedes ownership checks:
        // the error must describe the variables response, not contention.
        let error = apply_pull(&db, &file, Some(&invalid_vars), true, false).unwrap_err();
        assert!(error.to_string().contains("variableCollections"));
        assert_eq!(st.get(&crate::model::Id::Meta), meta);
        assert_eq!(st.get(&crate::model::Id::MirrorConfig), config);
    }

    fn synthetic_file() -> Value {
        serde_json::json!({
            "name": "Synthetic", "version": "1", "lastModified": "2026-01-01T00:00:00Z",
            "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT"}
        })
    }

    fn variables_export() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/variables-export.json")).unwrap()
    }

    fn snapshot_source(db: &Db) -> Option<String> {
        let st = db.open(|path| crate::open_store!(path)).unwrap();
        st.rtx(|(.., snapshot)| snapshot.get(&0).map(|s| s.source))
    }

    /// NWS-2961: the provenance row rides in the pull's transaction but not
    /// its churn — a first Enterprise pull counts only real records, and an
    /// identical re-pull is all-unchanged with nothing added.
    #[test]
    fn enterprise_snapshot_row_is_excluded_from_pull_churn() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db {
            path: dir.path().join("db"),
            key: None,
            layout: None,
        };
        let file = synthetic_file();
        let vars = variables_export();

        let (plain, ..) = apply_pull(&db, &file, None, false, false).unwrap();
        let file_recs = plain.added;
        let (first, ..) = apply_pull(&db, &file, Some(&vars), true, false).unwrap();
        assert_eq!(
            first,
            Churn {
                added: file_recs + 5, // + 2 collections + 3 variables, no snapshot row
                changed: 0,
                removed: 0,
                unchanged: 0,
            }
        );
        assert_eq!(snapshot_source(&db).as_deref(), Some("enterprise_rest"));

        let (again, ..) = apply_pull(&db, &file, Some(&vars), false, false).unwrap();
        assert_eq!(
            again,
            Churn {
                added: 0,
                changed: 0,
                removed: 0,
                unchanged: file_recs + 5,
            },
            "identical Enterprise re-pull must report zero churn"
        );
    }

    /// The first Enterprise pull after upgrading, onto a store that already
    /// holds the same variables but no snapshot row, adds nothing.
    #[test]
    fn enterprise_pull_onto_pre_snapshot_store_reports_zero_added() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db {
            path: dir.path().join("db"),
            key: None,
            layout: None,
        };
        let file = synthetic_file();
        let vars = variables_export();
        let (plain, ..) = apply_pull(&db, &file, None, false, false).unwrap();
        {
            let mut st = db.open(|path| crate::open_store!(path)).unwrap();
            let recs = crate::vars::parse_variables_export(&vars).unwrap();
            st.wtx(|tx| {
                for (id, rec) in &recs {
                    tx.upsert(id, rec);
                }
            });
        }
        assert_eq!(snapshot_source(&db), None);

        let (churn, ..) = apply_pull(&db, &file, Some(&vars), false, false).unwrap();
        assert_eq!(
            churn,
            Churn {
                added: 0,
                changed: 0,
                removed: 0,
                unchanged: plain.added + 5,
            }
        );
        assert_eq!(snapshot_source(&db).as_deref(), Some("enterprise_rest"));
    }

    /// The pull path stays lenient per entry: a malformed `variables/local`
    /// entry doesn't fail the pull (only `import-variables` rejects it).
    #[test]
    fn enterprise_pull_tolerates_a_non_object_variable_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db {
            path: dir.path().join("db"),
            key: None,
            layout: None,
        };
        let mut vars = variables_export();
        vars["meta"]["variables"]["VariableID:999"] = serde_json::json!(7);
        let (churn, ..) = apply_pull(&db, &synthetic_file(), Some(&vars), false, false).unwrap();
        assert_eq!(churn.removed, 0);
        assert_eq!(snapshot_source(&db).as_deref(), Some("enterprise_rest"));
    }

    #[test]
    fn rate_limited_waits_max_of_interval_and_retry_after() {
        let mut backoff = BACKOFF_START;
        let err = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(90),
        });
        // retry_after exceeds interval: use retry_after.
        let wait = pull_failure_wait(&err, &mut backoff, Duration::from_secs(10));
        assert_eq!(wait, Duration::from_secs(90));
        // rate-limit waits don't consume the exponential-backoff budget.
        assert_eq!(backoff, BACKOFF_START);

        let err = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(3),
        });
        let wait = pull_failure_wait(&err, &mut backoff, Duration::from_secs(10));
        assert_eq!(wait, Duration::from_secs(10));
    }

    #[test]
    fn other_errors_back_off_exponentially_and_cap() {
        let mut backoff = BACKOFF_START;
        let interval = Duration::from_secs(10);
        let net_err = PullError::Api(ApiError::Network("down".into()));

        let w1 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w1, Duration::from_secs(5));
        let w2 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w2, Duration::from_secs(10));
        let w3 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w3, Duration::from_secs(20));

        // non-Api errors (e.g. flatten failures) get the same treatment.
        let other_err = PullError::Other("bad shape".into());
        let mut backoff2 = BACKOFF_CAP / 2 + Duration::from_secs(1);
        let w = pull_failure_wait(&other_err, &mut backoff2, interval);
        assert!(w <= BACKOFF_CAP);
        assert_eq!(backoff2, BACKOFF_CAP);
    }

    #[test]
    fn pull_error_display_matches_prior_stringified_messages() {
        let e = PullError::Other("FIGMA_TOKEN not set — required for network pulls".into());
        assert_eq!(
            e.to_string(),
            "FIGMA_TOKEN not set — required for network pulls"
        );

        let e = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(30),
        });
        assert_eq!(
            e.to_string(),
            ApiError::RateLimited {
                retry_after: Duration::from_secs(30)
            }
            .to_string()
        );
    }
}
