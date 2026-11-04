# figmog — current-state spec

This is the authoritative description of figmog as the code exists today.
It supersedes `docs/history/*`, which are the retired build-design specs
and plans (bench, REPL, `--interactive`, `figmog watch`, human-mode CLI
output — all removed) kept only for provenance. No version archaeology
here: where this document says "figmog does X", that's a claim checked
against `src/` while writing it, not a claim about some past milestone.

## 1. What figmog is

figmog is a fold-backed local mirror of one or more Figma files. `pull`
fetches a file once and materializes it into deterministic, indexed
records in an embedded store (via `fold`'s `KeyedStream`); every read
after that — search, tree walks, component/style/variable/structural
queries — answers from local storage in milliseconds, at zero Figma API
cost. `figmog serve` runs the same engine as a long-lived MCP stdio
server with its own poll loop, plus a desktop proxy to Figma's native
desktop Dev Mode MCP server, so it can be the *only* Figma MCP an agent
needs to connect. `serve` additionally listens on a unix-socket control
plane (§6a), so CLI reads answer from the running server instead of
failing on the store lock it holds.

Two commands spend Figma's API budget deliberately and explicitly: `pull`
(and the pull paths `serve` drives) and `figmog images` (§15).

## 2. Architecture

```
                         ┌─────────────────────────┐
  Figma REST API  ─────▶ │  api.rs (FigmaApi)      │  Tier-1 file, Tier-3 meta,
   (figma.com)           │  UreqApi / test fakes    │  Tier-2 variables/local
                         └────────────┬─────────────┘
                                      │ Value (raw file JSON)
                                      ▼
                         ┌─────────────────────────┐
                         │  flatten.rs              │  pure: Value -> Vec<(Id, Rec)>
                         │  (+ vars.rs import path)  │  deterministic, no I/O
                         └────────────┬─────────────┘
                                      │ Flattened { recs, file }
                                      ▼
                         ┌─────────────────────────┐
                         │  store.rs                │  figmog_pipeline! (fold)
                         │  sync() — upsert + sweep │  KeyedStream<Id, Rec, _>
                         └────────────┬─────────────┘
                                      │ 16 materialized sinks (frozen names)
                                      ▼
                         ┌─────────────────────────┐
                         │  query.rs                 │  one source of truth for
                         │  status/pages/tree/...    │  every read answer
                         └───────┬─────────┬─────────┘
                                 │         │
                    ┌────────────┘         └────────────┐
                    ▼                                    ▼
         ┌─────────────────────┐              ┌─────────────────────────┐
         │  cli/ (read.rs)      │              │  dispatch.rs              │
         │  pretty-JSON stdout, │              │  dispatch_read_tool +      │
         │  or a socket client  │              │  tool_registry (21 tools)  │
         │  of serve (§6a)      │              └────────────┬────────────┘
         └─────────────────────┘                           │
                                                              ▼
                                              ┌─────────────────────────┐
                                              │  serve.rs / sessions.rs   │
                                              │  MCP stdio loop, one       │
                                              │  FileSession per mirror,   │
                                              │  poll loop, desktop proxy  │
                                              │  to Figma desktop MCP      │
                                              └─────────────────────────┘
```

`cli/pull.rs` and `sessions.rs` both drive the `api.rs -> flatten.rs ->
store.rs` path independently (one opens a single store at a CLI-resolved
path; the other opens one store per mirrored file) — see §5.

**HTTP deadlines:** every Figma REST request and signed image download
has a two-minute overall HTTP budget, covering response headers and the
entire success or error body, including redirects. The same absolute
deadline is enforced before each raw TLS socket read/write, including
handshakes and fragmented encrypted records; a slow trickle cannot renew
that budget. Redirects are followed explicitly, with each TCP connection
limited to the smaller of 30 seconds and the remaining overall budget.
These are per-request bounds, not a deadline for a whole pull
or multi-image tool call; ureq's blocking DNS lookup cannot be interrupted
by its HTTP deadline. Pull and metadata requests run on per-file workers
in `serve`, so stalled peers do not delay local/control-plane dispatch.
Image cache misses and desktop proxy calls retain their synchronous path.

`webpki-roots` and `url`, already transitive ureq dependencies, are direct
dependencies for this transport: the TLS wrapper uses the same Mozilla
trust roots as ureq, and redirect handling needs standard relative-URL
resolution. No TLS verification is disabled, and no worker is abandoned
after a timeout.

## 3. Data model

Every mirrored record is keyed by an [`Id`](../src/model.rs) and carries a
matching [`Rec`](../src/model.rs) variant:

| `Id` variant | `Rec` payload | source |
|---|---|---|
| `Node(String)` | `NodeRec` | every node in the document tree |
| `Component(String)` | `ComponentRec` | the file's `components` map |
| `ComponentSet(String)` | `ComponentSetRec` | the file's `componentSets` map |
| `Style(String)` | `StyleRec` | the file's `styles` map (metadata only — see §7) |
| `Variable(String)` | `VariableRec` | Enterprise auto-sync or `import-variables` |
| `VariableCollection(String)` | `VariableCollectionRec` | same as above |
| `Meta` | `FileMeta` | one row: name, version, last_modified, synced_at_unix_ms |
| `ProxyCache(String)` | `ProxyCacheRec` | legacy proxied-tool responses (§8) |
| `MirrorConfig` | `MirrorConfigRec` | one row: this mirror's sticky pull settings (§14) |
| `ImageBlob(String)` | `ImageBlobRec` | cached render and fill bytes (§15) |

`Id`/`Rec` are **append-only enums** — postcard encodes variant indices
positionally, so inserting a new variant anywhere but the end would
silently corrupt every existing on-disk store. `ImageBlob` is the most
recent addition and sits last in both enums (`MirrorConfig` immediately
before it); the next new record kind goes after `ImageBlob`, never
between existing variants. The same rule is why a new setting arrives as
a new record kind rather than a new field on an existing `Rec` struct:
struct fields are positional too, so widening `FileMeta` would break
every store already on disk, while appending `MirrorConfig` does not.

`NodeRec` is the workhorse record: id, parent/child-index, page id
(nearest CANVAS ancestor), type, name, visibility, TEXT `characters`,
INSTANCE's `component_id`, sorted `component_properties`, `style_refs`
(sorted `(style_type, style_id)` pairs), `bound_variables` (sorted
`(json-pointer, variable_id)` pairs — the pointer addresses the *resolved
value* location, e.g. `/fills/0/color`), `abs_bounds`, and `raw` (the
node's canonical JSON with `children` stripped, used for `figmog where`
and style-value derivation).

`ProxyCacheRec` (§8): `key_hash` (= the `Id::ProxyCache` key), `tool`,
`args_canonical`, `file_version`, and `content` (canonical JSON of the
upstream result), retained for on-disk compatibility. Desktop calls no
longer read or write these rows (§8). The legacy lookup helper requires
`tool`/`args_canonical` to match the *request*, not just the key hash —
FNV-1a 64 is non-cryptographic and collidable, and tool arguments are agent-authored strings, so the key
hash alone is not trusted (see `cache::lookup`).

`MirrorConfigRec` (§14): `geometry: bool`, one row per mirror. An absent
row means `false`, which is what every store written before this record
kind existed reports.

`ImageBlobRec` (§15): `key_hash` (= the `Id::ImageBlob` key), `kind`
(`"render"` or `"fill"`), `subject` (node id for a render, `imageRef`
hash for a fill), `format`, `scale_milli` (requested scale × 1000, always
`1000` for fills), `file_version`, and `bytes`. A hit checks
`kind`/`subject`/`format`/`scale_milli` against the request rather than
trusting the key hash, the same identity check `cache::lookup` makes for
proxied responses.

**Determinism contract:** every map-shaped field is a sorted `Vec` of
pairs; every canonical-JSON string comes from `serde_json` without
`preserve_order` (`serde_json::Value` is `BTreeMap`-backed here, so
`to_string` is already sorted-key deterministic); two flattens of
byte-identical file JSON must produce byte-identical postcard-encoded
records, because `KeyedStream::upsert` uses byte equality as its change
detector. No `HashMap` iteration ever reaches an output boundary.

## 4. Pipeline and sinks

`store::figmog_pipeline!()` wires the flattened records into 16
`fold` terminal sinks, keyed off `store.rs`'s pure branch functions
(`node_only`, `child_edge`, `text_doc`, `instance_edge`, `style_edges`,
`variable_edges`, `type_edge`, plus one `rec_branch!`-generated filter per
non-node table). **Sink names are frozen on-disk schema** — renaming one
is a breaking store-format change:

| sink | kind | keyed by | feeds |
|---|---|---|---|
| `nodes` | Table | node id | every node read |
| `children` | Multimap | parent id → `(child_index, child_id)` | `tree`, `get --children` |
| `text` | Bm25 | node id → name + characters | `search` |
| `instances_of` | InvertedIndex | node id → component id | `instances` |
| `styled_by` | InvertedIndex | node id → style id | `uses`, `styles` |
| `bound_to` | InvertedIndex | node id → variable id | `uses`, `vars` |
| `by_type` | InvertedIndex | node id → Figma type | `find`, `pages`, `text`, `stats` |
| `components` | Table | node id | `components`, `instances` |
| `component_sets` | Table | node id | `components`, `instances` |
| `styles` | Table | style id | `styles`, `uses` |
| `variables` | Table | variable id | `vars` |
| `variable_collections` | Table | collection id | `vars` |
| `meta` | Table | `0u8` (not `()` — postcard-encodes to zero bytes, and the store forbids empty keys) | `status`, version comparisons |
| `proxy_cache` | Table | cache key hash | legacy proxied-tool cache (§8) |
| `mirror_config` | Table | `0u8` (one row, like `meta`) | sticky pull settings (§14) |
| `images` | Table | image cache key hash | render/fill byte caching (§15) |

`mirror_config` and `images` are appended at the pipeline root's tail, so
the reader tuple grows at the end and older stores keep decoding. Both
sit outside `collect_sweepable` (§5): a file sync never removes a config
row, and image rows are evicted by version instead.

`bound_to`'s edges are deliberately not deduped before feeding the
inverted index: `InvertedIndex` is set-semantic, so duplicate `(node,
var)` edges (a variable bound at multiple property paths on the same
node) collapse harmlessly to the same membership.

## 5. Sync, churn, and sweep

`store::sync` applies one flattened file in a single write transaction:
upsert every record and the meta row, then remove ids that were live
before this sync but are absent from it now (the sweep). It returns a
`Churn { added, changed, removed, unchanged }` count.

**What's sweepable:** nodes, components, component sets, and styles
always are (`collect_sweepable`, read via `rtx` *before* `sync`, since
this is a single-writer process — no write races). Variables and
variable collections are swept only when the caller opts them in for
that cycle via `collect_variable_ids` — i.e. only on a pull that actually
fetched an Enterprise `variables/local` export this time. On a
non-Enterprise or `--from-file` pull, stored variables sit entirely
outside `sync`'s live/sweep accounting: manually imported records persist
across ordinary pulls indefinitely (§9). The meta row is never
sweepable.

**Idempotence:** re-syncing byte-identical records is a no-op through the
whole pipeline — `unchanged` increments, no index writes happen. A
spurious watch trigger or a repeated `pull` therefore costs exactly one
Tier-1 fetch and nothing else downstream.

**Version-keyed cache eviction** is a separate pass, not folded into
`sync`'s sweep (its churn accounting stays untouched by this feature):
after a sync whose file *version* actually moved, every `proxy_cache` row
(`store::stale_cache_ids`) and every `images` row
(`store::stale_image_ids`) tagged with the old version is removed through
the one `store::evict_stale_cache` function. This runs after every
`pull`/`figmog_sync`/watch-tick pull that changes the version, and is
bundled into `pull --fresh`'s whole-store wipe too.

**Tree validation:** the shared flattener rejects document roots that are
not objects with a non-empty string `id` and `type: "DOCUMENT"`. Every
descendant must also be an object with non-empty string `id` and `type`
fields; unknown descendant types remain supported. A node's `children`,
when present, must be an array of valid nodes. Missing `children` and empty
arrays are accepted, including on an empty DOCUMENT root. Invalid trees
return an error with the node's location, before an ordinary CLI import,
fresh replacement, or server pull can sync a partial tree or sweep existing
subtrees. Other fields remain unknown-field-preserving.

**Fresh replacement:** file and optional variables responses are validated
before opening or resetting the mirror. Every direct CLI and server-session
store handle holds a lock on the permanent `.figmog-lock` file inside the
database directory until the store closes. The anchor is a regular file,
owned by the current user, mode 0600 on creation, with no hard links or
symlink following; its directory is protected with mode 0700. Every view of
the physical mirror opens the same anchor, including case/Unicode-equivalent
spellings and filesystem namespaces with isolated `/tmp` directories.
A fresh pull first opens the existing store, checking fjall's own writer
lock too, then holds the anchor while closing, clearing every other entry,
and reopening the store. Both the directory and anchor stay linked; resets
preserve the anchor by filesystem identity, not filename spelling. Symlinks
among the discarded entries are unlinked without following their targets.
Reads, ordinary pulls, server sessions, and fresh resets need no write access
to the database's parent and no administrator-provisioned sibling file.
A running serve makes a valid fresh pull fail with a clean ownership error;
invalid input leaves the existing mirror untouched. Deletion errors are
reported. Never remove the anchor while a process owns the mirror; its OS
lock releases when the handle closes or the process exits. This does not
promise recovery from an unopenable/corrupt store or rollback after filesystem
failures during reset.

## 6. `figmog serve`: multi-file sessions, watch, MCP

One `figmog serve` process owns every mirrored file's store via a
[`SessionManager`](../src/sessions.rs). Each `FileSession` has one background
worker owning its only writer and watch watermarks. Dispatch reads an owning
fold snapshot, independently of the writer. A new snapshot is published only
after the whole fetch → flatten → atomic sync → cache eviction → geometry
persistence cycle succeeds. During every stage, existing readers see the
previous complete mirror, including for the file being pulled. Other files
have independent workers. No on-disk records or sink names change.
An unchanged stored version skips both stale proxy-cache and image-cache
scans, avoiding decoding cached image blobs. Initial or version-changing
pulls still scan both tables and evict mismatched rows before publication.

**Asynchronous pulls (NWS-2919):** `figmog_open {file, geometry?}` and
`figmog_sync {file?}` acknowledge accepted work immediately:

```json
{"accepted":true,"key":"<file-key>","job_id":1,"state":"running"}
```

`state` in this acknowledgement is `queued` or `running`; it is not a
successful pull result. Job ids increase per file for this process's lifetime;
identify a job by `(key, job_id)`. These tools advertise an acknowledgement
output schema and return both MCP text and `structuredContent`.

Poll `figmog_status {file}` or `figmog_files` for `ready` and `pull`:

```json
{
  "ready": true,
  "pull": {
    "job": {"id":1,"state":"succeeded","geometry":false,"error":null,"phase":null},
    "queued": null,
    "polling": false,
    "last_completed": {"id":1,"state":"succeeded","geometry":false,"error":null,"phase":null},
    "last_success": {"job_id":1,"name":"Example","version":"42",
                     "churn":{"added":1,"changed":0,"removed":0,"unchanged":0}}
  }
}
```

A job moves from `queued` to `running` to `succeeded` or `failed`.
`polling` reports an in-flight watch metadata poll. Running `phase` is `fetching`, `flattening`, `applying`, or `publishing`.
The latest terminal job is retained as `last_completed`, and
`last_success` retains the most recent successful version and churn even
when a later job fails. A reopened existing mirror has `ready: true` and
its persisted version/time; in-memory job history starts empty. A terminal
`error` carries the actionable pull error (credentials, HTTP/Retry-After,
parse error, etc.). Failure leaves the prior published snapshot readable.
Before the first successful mirror, status/files remain available with
`ready: false`, null metadata and zero nodes; data reads return a clear
not-ready error pointing at these tools rather than an empty answer.

Duplicate open/sync requests coalesce onto the active or queued job id.
There is at most one worker operation and one queued pull per file. A
`geometry: true` request arriving after a non-geometry pull started reserves
one follow-up job; further duplicates share it. Geometry stays requested
across retries and persists after a successful pull. Other follow-up changes
are detected by the next metadata poll; repeated syncs during a running job
do not create an unbounded backlog. Each file's pull retries honor
Retry-After or exponential backoff, including explicit requests made during
the wait; other files continue independently. A failed first mirror can be
retried by open/sync, another data read (including `figmog_images`), or watch reconciliation. Status/files
do not erase terminal errors by implicitly retrying them.

Polls and pulls execute serially on the file's worker. No poll observed after
a fetch can certify it, and no stale/out-of-order completion can overwrite
a newer publication. The main loop consumes each completion once and only
then updates the view, version/churn and default file's `.figmog/current`.
A socket timeout/disconnect does not cancel accepted work. Stdin EOF or a
broken stdout pipe shuts down the server: it stops accepting work, discards
control-plane queued follow-ups, and joins workers after commands already
sent to them finish. Forced process termination may interrupt work; fold's
atomic transaction protects the store. Job history is not persisted.

**Dependency justification:** `fjall` and `self_cell` were already transitive
locked dependencies. Direct `fjall` use names fold's snapshot type; it is
pinned to the version used by fold, with default features disabled.
`self_cell` safely owns that snapshot with its borrowing fold readers,
without unsafe application code or leaked snapshots. The small
`SharedPipeline` adapter uses only fold's pinned public API and keeps its
keyed writer, pipeline and schema unchanged. These are the justifications
to carry into a PR adding these direct dependencies.

**Startup:** `figmog serve [FILE]...` takes zero or more file
URLs/keys. Zero is valid and needs no `FIGMA_TOKEN` up front — the server
starts empty and mirrors files as an agent references them. The first
startup `FILE` (if any) becomes the *default* file for any tool call that
omits `file`; with none given but exactly one file later auto-opened,
that one becomes the effective default (`SessionManager::effective_default_key`)
— two or more auto-opened files with no startup default leaves no
default at all, and an omitted `file` then errors, naming
`figmog_open`/`figmog_files`.

Startup opens stores before serving and queues an initial pull for each
missing mirror when watch is enabled. It never waits for network pulls;
those failures appear in job state while the server stays available.
`--no-watch` queues no startup pulls. Invalid paths, store locks and required
watch credentials still fail startup. Explicit first references queue the
same asynchronous lifecycle, regardless of watch mode.

**Reader loop:** background readers send stdin and socket frames through
one `mpsc` channel. The main loop handles due watch work and completions
before each request, even under continuous traffic. It wakes at most 20ms
later to consume worker completions, including under `--no-watch`.
Stdout writes are manual (`writeln!` + flush), so a vanished stdio reader
causes clean shutdown instead of a panic. Desktop proxy calls and image
cache-miss networking retain their existing synchronous behavior; they are
outside this pull-scheduling change. When a file's worker is occupied by a
pull or metadata poll, `figmog_images` returns a prompt busy error (including
for cache hits), rather than waiting behind that operation.

**Watch/poll loop:** normal rounds visit one session every
`max(--interval / session_count, 2s)`. Ordinary polls and retries share a
two-second global dispatch floor (≤ 30 requests/min). The session's
worker performs the blocking HTTP call. Busy files are skipped, and each
file retains its own retry/poll deadline, so a delayed poll or rate-limited
file never pauses other files' reads or polling. `--interval` is clamped
to a one-day maximum. On `Tick::Changed`, completion queues a pull through
the same lifecycle as explicit open/sync. Failure leaves the observation
pending for retry; Retry-After or exponential backoff (capped at 5 minutes)
is per file. Outstanding metadata and pull failures wake the scheduler at
their per-file retry deadline, independently of the next normal round.
When normal and retry work are both due, they alternate so neither starves.
Busy workers and explicitly queued pulls remain serialized. A recovery
poll does not postpone its next normal visit, and a retry can also satisfy
the same file's already-due normal visit. `--no-watch` disables automatic
ordinary retries, while explicit and expedited requests retain their
existing behavior.
Ordinary poll eligibility is anchored to metadata request dispatch, matching
the global round-robin clock. An unchanged completion or successful pull
publication does not move it later and accidentally skip the next round.
Metadata retry deadlines and pull retry deadlines remain separate from this
ordinary cadence; neither can be bypassed by an expedited request. A
successful pull does not cancel outstanding metadata recovery or reset its
exponential backoff.

**Watch watermarks:** each session keeps a successfully synchronized
metadata `last_touched_at` and a separate pending observation in memory.
A changed poll records the pending timestamp; only a successful pull
acknowledges it. Repeated polls, metadata errors, and failed pulls do not
acknowledge a pending change. A successful pull never replaces this
metadata watermark with full-file `lastModified`, since the endpoints
can report different timestamps for unchanged content. Comparison keeps
the complete strings, including fractional seconds, so distinct edits
within one second still trigger pulls.

Every newly opened session (including a reopened on-disk mirror or an
immediate startup pull) starts with an unknown metadata watermark: it is
not persisted in `FileMeta` or any other record. Its first successful
metadata poll therefore triggers one reconciliation pull. Thereafter an
unchanged session needs no more Tier-1 fetches. Explicit `figmog_open`,
`figmog_sync`, and auto-open pulls use the same success/failure bookkeeping:
when an observation is pending, success acknowledges that pre-fetch
timestamp; otherwise success retains the previous watermark, possibly
unknown. A subsequent poll may require one reconciliation pull if an
explicit sync fetched a change that had not yet been polled. No metadata
request after a fetch certifies the fetched content: an edit racing any
pull remains detectable by the next poll, even if it happened within the
same second. This needs no extra metadata requests or on-disk schema changes.

**`file`-argument routing (spec-frozen tool surface, v4):** every local
tool's JSON schema gains an optional `file` property (URL or bare key).
Explicit `file` routes to that mirror, opening it and queuing a Tier-1 pull
if it has no successful mirror. A first data read reports not-ready until
that job succeeds. Failed sessions remain available for inspection/retry;
retries coalesce and honor the file's backoff. Omitted `file` routes
to the default (see "Startup" above). There is no idle-session eviction
and no cross-file queries — once opened, a session stays open for the
process's life.

**The 21 `figmog_*` tools** (`dispatch::tool_registry`): 13 core reads
(`figmog_status`, `figmog_pages`, `figmog_tree`, `figmog_node`,
`figmog_subtree`, `figmog_find`, `figmog_search`, `figmog_instances`,
`figmog_components`, `figmog_styles`, `figmog_uses`, `figmog_vars`,
`figmog_sync`) + 5 whole-file structural queries (`figmog_stats`,
`figmog_path`, `figmog_text`, `figmog_where`, `figmog_at`) +
`figmog_images` (§15) + 2 multi-file management tools that aren't
per-file (`figmog_open`, `figmog_files`). Every tool but
`figmog_sync`/`figmog_open`/`figmog_images` reads the local mirror at
zero Figma API cost; `figmog_sync` and `figmog_open` schedule pulls, and `figmog_images` fetches renders and fills, so
those three are the local tools that spend Figma's rate budget.
`figmog_status`'s result also carries
`upstream: "connected" | "unreachable" | "disabled"`, spliced in at the
call site that knows it (never inside `query::status` itself).
`figmog_open` also takes an optional `geometry` boolean (§14).

The 19 per-file tools each carry the optional `file` property described
above; `figmog_open` and `figmog_files` don't.

**Protocol (`mcp.rs`):** pure JSON-RPC 2.0 over stdio frames, no store,
no I/O. `initialize` echoes the client's `protocolVersion` (or
`2025-06-18` if omitted) and returns steering `instructions` telling an
agent to reach for figmog for everything Figma-related. A request with no
`id`, or a `notifications/*` method, gets no response frame. Local tool
results are wrapped as a single text content block
(`ToolOutput::Json`); proxied results are emitted verbatim
(`ToolOutput::Raw`) since they're already a complete, correctly-shaped
MCP `CallToolResult` (re-wrapping would double-encode a non-text content
type like `get_screenshot`'s image block).

## 6a. The unix-socket control plane

`figmog serve` binds `<figmog-root>/serve.sock` (mode 0600, inside a root
tightened to 0700) before it opens any session store, and unlinks it on
exit through a guard held for `run_serve`'s whole scope. A pre-existing
socket file is probed with a connect first (`serve::classify_probe`):
connect succeeds ⇒ another `serve` owns this root, and this process exits
with the standard `{"error": ...}` JSON; any connect failure ⇒ the file
is stale, so it is unlinked and rebound, except invalid addresses (including
AF_UNIX address-length errors), which fail without unlinking. Connect and
bind use a short relative address when the verified root is beneath the
working directory; permissions and cleanup use the verified absolute path.
This preserves default socket startup in deep checkouts. `--no-socket`
skips binding.

Frames on the socket are the same newline-delimited JSON-RPC the stdio
loop reads. `initialize` is optional for socket clients: `tools/call` is
accepted directly, since dispatch treats every frame independently.
An acceptor thread gives each connection its own reader
thread, which tags frames with a connection id and forwards them into the
same `mpsc` channel stdin feeds; the connection's write half is
registered before its reader starts, so a response always has somewhere
to route. Responses are written to the owning connection under a write
timeout, so a client that stops reading is disconnected and dropped from
the registry rather than stalling the single-threaded loop; a vanished
client is a silent no-op. The store stays single-threaded throughout:
socket traffic interleaves with MCP frames and watch ticks like any other
request.

Socket `tools/call` registry and handler failures (including upstream
protocol/transport failures) use a top-level JSON-RPC `error` with code
`-32000`, so the CLI emits its standard stderr error and exits 1.
Genuine upstream results remain verbatim `result` values, including
tool-level `isError: true`; those are successful forwarding operations.
Stdio retains MCP `isError` content for registry and handler failures.

The CLI side of the plane is in §10.

## 7. Style values: derived, not authoritative

The file JSON's `styles` map is metadata only (id, name, type,
description) — not the style's actual properties. `figmog styles
--values` (`values: true`) derives a definition from one consumer node's
resolved `raw` JSON (`vars::style_value_from_consumer`: `/style` for
TEXT, `/fills` for FILL, `/effects` for EFFECT, `/layoutGrids` for GRID).
A style with no current consumers has no derivable value.

## 8. The desktop proxy

Unless `--no-upstream`, `figmog serve` (and CLI parity via `figmog
tools`/`figmog call`, §10) probes Figma's native desktop app's Dev Mode
MCP server (`http://127.0.0.1:3845/mcp` by default, streamable HTTP) at
startup. This requires a **paid Dev/Full seat** and the desktop app
running with Dev Mode MCP enabled. On success, `tools/list` merges
figmog's 21 local tools with every upstream tool verbatim (description
prefixed `"[via Figma desktop] "`); on failure, one stderr line and
local-only tools for the rest of the process — **no mid-session
re-probe**, so a server that starts before the desktop app is reachable
stays local-only until restarted.

**Namespace rule:** `figmog_*` names are always local; every other name
is proxied. `merge_registry` drops (and logs) any upstream tool whose
name collides with an actual local tool name — not just the `figmog_`
prefix — or with another upstream tool already accepted in the same
merge; this can't happen from figmog's own registry, but a live desktop
server's tool list is outside figmog's control.

**Desktop context and caching:** the desktop answers for the file active
in Figma, independently of every mirror figmog manages. Its MCP interface
does not supply authoritative file identity and revision for a call, so
all proxied calls forward **uncached**, including `get_*`/`list_*` calls
with explicit node ids. A node id can occur in multiple files; a caller's
`file` argument or the default mirror's version cannot establish which
file/revision the desktop will answer from. Switching files or editing the
active file must therefore reach upstream even if the mirror is unchanged.
This applies equally to stdio, socket-routed CLI calls, direct `figmog call`,
and servers with no mirrors. Arguments and results (including tool-level
`isError: true` results) pass through verbatim.

Legacy `proxy_cache` rows are never served or populated by desktop calls.
The record layout, enum variants, sink name, and eviction helpers remain
for on-disk compatibility; ordinary version-change eviction and
`pull --fresh` can still remove old rows. Any future proxy caching must
first establish authoritative upstream file identity **and** freshness;
routing to a mirror alone is insufficient. `figmog tools` reports
`cacheable: false` for desktop tools on both direct and socket paths.

**Rate budget:** native-named calls reach the desktop server every time;
`figmog_*` read tools continue to answer locally at zero API cost. A
successful proxied call to a tool that isn't `get_*`/`list_*` (a likely
write) records an expedited meta-poll for each known session rather than
waiting for the next `--interval` tick: the desktop supplies no authoritative
active-file identity with which to select a single mirror. Repeated writes
coalesce per file. A request made during a pull or poll remains pending
until the worker is idle; a write after a metadata request started requires
another observation. Expedited polls bypass the ordinary interval, retain
their pending refresh after a transient failure, and honor both metadata
and pull Retry-After/backoff deadlines. They are skipped under `--no-watch`.
The ordinary round-robin rate bound excludes these write-triggered polls.

## 9. Variables

Three layers, each covering what the one above can't:

1. **Enterprise auto-sync (automatic, zero setup).** Every network `pull`
   additionally calls `GET /v1/files/:key/variables/local` — Enterprise
   only. `Ok(Some(_))` folds the full-fidelity collection/variable
   records into the same sync, kept live (swept like any other file
   state, §5). `Ok(None)` (403/404 on a non-Enterprise plan) is not an
   error and is silently skipped, falling through to the two paths below.
   `--from-file` pulls never call it (no network involved).
2. **Mirrored bindings + inference (always on).** Every node's
   `bound_variables` plus Figma's own baked-in resolved value are scanned
   at read time (`vars::infer_from_nodes`) into per-variable usage
   (binding sites) and observed values. This covers each variable's
   default-mode value only; a non-default mode's value is visible only
   where a frame explicitly overrides that mode.
3. **Manual import (optional).** `figmog import-variables <export.json>`
   (`vars::parse_variables_export`) upserts the same full-fidelity
   records auto-sync would, from either the Enterprise REST shape or the
   bare object a Figma plugin-console export produces (any plan, via the
   Plugin API run from Figma's own developer console). Unlike auto-synced
   records, manually imported ones are **not** swept by a later pull with
   no Enterprise export of its own — they persist until re-imported or
   `pull --fresh` (which wipes the whole store, imports included).

`figmog vars`/`figmog_vars` prefer an authoritative record (auto-synced
or imported) over inference whenever one exists for a given variable id.

## 10. CLI surface

JSON is the CLI's only output mode (there is no `--json` flag — there's
nothing to select between). Every command prints pretty-printed JSON on
stdout via one seam (`cli::write_json`); every failure prints
`{"error": "..."}` on stderr and exits 1 (a raw internal panic exits 101
instead, still as one JSON stderr line — see `open_store_checked`'s doc
comment in `src/cli/mod.rs`). A reader that vanishes mid-write (piped
into `head`, a killed consumer) is a silent, clean exit 0, never a panic
or a broken-pipe message.

`figmog version` reports the installed package version as `{"version": "..."}`
without resolving a mirror, opening a store, or accessing credentials.
The standard clap flags `--version` / `-V` print `figmog <version>` as plain
text and exit before dispatch, like `--help`.

The CLI is split by concern: `src/cli/mod.rs` (clap types, top-level
dispatch, `run`, the shared store-opening/JSON-writing helpers),
`src/cli/pull.rs` (`pull`, the typed `PullError`, `.figmog/current`),
`src/cli/read.rs` (the 17 read-only query commands), `src/cli/call.rs`
(`tools`, `call`, `import-variables` — the desktop-proxy CLI parity
surface, so every tool `figmog serve` would expose is reachable without
an MCP client), `src/cli/images.rs` (`images`, §15), and
`src/cli/socket.rs` (the client half of the control plane, §6a).
`src/cli/auth.rs` handles credential commands and hidden terminal input;
`src/auth.rs` owns credential resolution and native storage.

`figmog tools` never opens the store (it only needs an optional upstream
probe), and `figmog auth` manages credentials independently. Neither
requires a resolved mirror — no established `.figmog/current` and no
`--db` needed. `figmog call` does need one, since local tool dispatch
reads the store.

Default mirror paths are `.figmog/<key>/db`. Persisted `.figmog/current`
contents are trimmed and must be a bare key of at least 10 ASCII
alphanumeric characters; nonempty malformed contents fail before opening
or deleting a store, including during socket routing. Path construction
also checks that a key is a single normal path component. Explicit pull
file references still accept Figma URLs, and `--db` selects its given path
without consulting `.figmog/current`.

Before every direct CLI or server-session store open, figmog creates the
database directory with mode 0700 and tightens an existing directory to
0700. This includes explicit `--db` paths, `--fresh` rebuilds, and serve
with `--no-socket`. Directory traversal is the privacy boundary: journal
and table files remain inaccessible to other OS users even if their own
modes permit reading. Existing permissive stores migrate on their next
open; no recursive data-file chmod is needed. Missing ancestors are
created with mode 0700; existing ancestors keep their modes. Entries created
concurrently are re-read and checked like existing entries. Every path
component must be owned by the current user or root. Group- or
other-writable ancestors are rejected unless sticky directory semantics
protect the owned child from replacement (as under `/tmp`). Ancestor
symlinks and their targets undergo the same checks; subsequent store
access uses the verified absolute path. Final symlinks are rejected even
with trailing `/` or `/.`. These checks also precede `--fresh` deletion.
Writing `.figmog/current` also protects `.figmog` itself. A failure to
protect a selected directory produces a clean JSON error before opening
the store or writing the pointer.

**Credentials.** `figmog auth login` reads a personal access token from a
hidden terminal prompt, or from stdin with `--token-stdin`. It rejects
empty, multiline, non-ASCII, and oversized tokens (4096-byte maximum),
stores one token per OS user under service `figmog`, account
`figma-token`, and returns `saved`, `storage`, and `environment_override`
as JSON. Storage is macOS Keychain or Linux Secret Service; there is no
plaintext-file fallback. Saving does not validate the token over the
network or require extra Figma scopes. `auth status` returns `configured`,
`source` (`environment`, `credential_store`, or null), and `validated:
false`, without disclosing the token. `auth logout` removes only the
saved credential, is idempotent, and reports `removed` plus whether an
environment override remains.

`pull`, `serve`, and direct image downloads use the same credential
loader: explicit `FIGMA_TOKEN` first, otherwise the saved credential.
An explicitly empty environment value disables saved credentials. A
locked/unavailable store is treated as no usable token for these callers,
so cached reads and token-free idle servers still work; `auth status`
reports the storage error for diagnosis. Credential changes take effect
on the next CLI invocation or server restart. Tokens remain subject to
Figma expiration/revocation; this is personal-token storage, not OAuth.

The new dependencies are justified by native secret storage (`keyring`)
and terminal echo suppression (`rpassword`), avoiding plaintext files,
secrets in argv, shell credential helpers, and hand-written terminal
handling. The Linux DBus client is vendored so release builds do not
require DBus development headers; an unlocked Secret Service is still
required at runtime for saved credentials. Automated tests use isolated
in-memory credentials and explicitly disable real credentials in child
processes. The ignored `auth::tests::native_credential_store_round_trip`
test exercises a disposable native entry when a store is available.

**Git ignore rules.** After a successful CLI `pull` into the default local
mirror (including `--from-file`), inspect ancestors up to the nearest
`.git` directory or file, recognizing worktree/submodule boundaries.
If Git does not already ignore `.figmog/`, append that pattern to the
closest existing `.gitignore` within the repository; create the root
`.gitignore` if absent. Preserve existing content and line endings.
Failed pulls, explicit `--db` paths, and directories outside Git do not
change ignore rules. Symlink/nonregular ignore files are not modified.
All in-repository ancestor ignore files are checked before invoking Git,
and the Git check is limited to two seconds so external exclude files
cannot stall a completed pull indefinitely. The query checks `.figmog`
without a trailing slash to distinguish a directory ignore from a
contents-only rule with descendant exceptions. Appends read and write
through one descriptor; on Unix, `O_NOFOLLOW` and `O_NONBLOCK` prevent
swapped symlinks/FIFOs from redirecting or blocking the open. `libc` is
used directly for these OS flags (it was already a transitive dependency).
Ignore-update failures emit a JSON warning on stderr without failing an
already successful pull. This does not untrack committed files or share
mirror data between worktrees.

**Socket-first routing (§6a).** When neither `--no-socket` nor `--db` is
given, every read command, plus `tools`, `call` and `images`, first tries
`<.figmog>/serve.sock`. Reachable ⇒ the command becomes a client of the
running `serve`: `cli::cmd_as_tool_call` maps the subcommand to the tool
name and argument object `dispatch::dispatch_read_tool` expects, so the
JSON on stdout is byte-identical to the direct-open answer, and a
serve-side failure surfaces as the standard `{"error": ...}` exit 1.
Unreachable ⇒ the store is opened directly. `--db` bypasses the socket
entirely, since it names a store rather than a root. Absent optional
flags are omitted from a mapped command's arguments. For `where`, an
omitted `--equals` tests pointer existence, while explicit `--equals null`
matches only present null values; the matching MCP tool distinguishes an
absent `equals` key from `"equals": null` the same way (`figmog call`'s own
`--args` payload is passed through untouched). Both direct
and socket reads for per-file local tools select an explicit `call`
argument `file` first (key or Figma URL), then the file inferred from a
node URL in `id`, `under`, or `target`, then `.figmog/current`.
An explicit `--db` takes precedence
over all of these. Current is injected as a routed `file` argument only
for per-file local tools, when no explicit non-null `file` was supplied
and no node URL selects a file. `figmog_open` and `figmog_files` keep
their management semantics: `file` names the file to open for the former,
and the latter lists all sessions without a routing target. A routed
`figmog_status` result drops the server-only `upstream`, `ready` and `pull`
fields, keeping ordinary CLI output byte-identical for an existing mirror.
Before a first successful mirror, plain `figmog status` returns a clear not-ready error;
`figmog call figmog_status` exposes the full lifecycle instead.

Desktop-proxy calls do not interpret `file` or node URLs as mirror
selectors (§8): direct mode opens the current mirror (or an explicit
`--db`), while serve forwards desktop calls without resolving a mirror.
Both paths forward uncached and preserve desktop arguments exactly,
including an absent `file` or explicit null, string, number, or object.
Socket routing never injects `.figmog/current` into desktop arguments.

`pull` is a writer and always opens the store directly. While `serve`
holds that store, `pull`'s lock error additionally says "or ask the
running serve: figmog call figmog_sync", which reaches the process that
owns it and returns an immediate job acknowledgement. Inspect
`figmog call figmog_status` or `figmog call figmog_files` for completion and
errors. With no running server, direct `figmog call figmog_sync` remains
synchronous and returns churn.

**Single-writer constraint:** fjall allows only one open handle per
store. `figmog serve` holds its `--db`'s lock for its whole life; a CLI
command that opens the same store directly (`--no-socket`, an explicit
`--db`, or `pull`) while `serve` runs gets a clean `store is locked — is
figmog serve running? ...` error (exit 1), never fold's raw lock panic
(exit 101) — see `open_store_checked`.

## 11. Determinism and schema-stability contracts

- All time comes through explicit call sites (`SystemTime::now` in
  `cli::pull::now_ms`, `Instant`/`Duration` in `serve.rs`/`watch.rs`) —
  there is no hidden wall-clock read inside the pure flatten/store/query
  layers.
- No `HashMap` iteration ever reaches an output boundary: every
  record's map-shaped field is a sorted `Vec`; JSON serialization never
  enables `preserve_order`.
- `Id`/`Rec` are append-only postcard enums (§3) — a store built by an
  older figmog binary must still open cleanly with a newer one, as long
  as no variant was reordered or removed.
- `Rec` payload structs are positional too, so new stored state arrives
  as a new record kind rather than a new field on an existing struct
  (§3, §14).
- Sink names (§4) are frozen on-disk schema; renaming one changes what
  store directory an unmodified binary can open. New sinks are appended
  at the pipeline root's tail, so reader tuples grow at the end.
- `fold`/`bogkit` is upstream: never vendored, never patched — figmog
  consumes only the pinned git dependency's public API (see the crate's
  `AGENTS.md`).

## 12. Node addressing

`ident::parse_node_ref(input) -> Option<(Option<file_key>, node_id)>` is
the one place a node-id argument is parsed. A bare id passes through with
`normalize_node_id`'s `0-1` ⇒ `0:1` rule and nothing else. A figma.com
URL carrying `node-id=` anywhere in its query yields that id
(percent-decoded, normalized) plus the URL's file key when the URL names
one; a figma.com URL with no `node-id=` yields `None` rather than being
treated as an id. `ident::normalize_node_ref` is the "just give me the
id" wrapper the query layer calls, falling back to the raw input.

Every node-id-shaped argument accepts both forms: the CLI's `id`,
`target`, `--under` and `--page` arguments, and the tools' `id`, `under`,
`target` and `page` properties. In `serve`, when a tool call omits `file`
and one of `id`/`under`/`target` is a URL that names a file,
`dispatch::infer_file_from_node_ref` routes the call at that file and
auto-opens it. An explicit `file` always wins; a disagreement between the
two is named in the error text if the node isn't found, comparing
normalized keys so two spellings of the same file don't false-positive.

## 13. Subtree dump, subtree scoping, resolved variables

**Subtree dump** (`query::subtree`; `figmog dump <id>`,
`figmog_subtree`): the node's full `raw` JSON with `children` nested
recursively in child-index order, to `depth` levels (unlimited when
omitted). `fields` projects every node to the named raw fields; `id`,
`name` and `type` survive projection by an explicit keep-list, and
`children` plus `resolved_variables` are inserted after projection runs,
so they survive too. An unknown field name is simply absent. Response
size is the caller's business: the tool description says so and points at
`depth`/`fields`.

**Subtree scoping** (`query::resolve_under` + `query::scope_ids`;
`--under <id>` on `text`/`find`/`search`/`where` and the matching tool
argument): one BFS over the `children` index from the scope root
(inclusive, cycle-safe via a visited set) collects the descendant id set,
and results are filtered against it. It composes with `page` by
intersection. An unknown scope id gives the standard `no node ...` error.
`search` orders all matching hits by score descending, then node ID
ascending (lexicographic), before applying the result limit. Scoped calls
filter before truncating, so an in-scope hit can't be lost to an
out-of-scope one taking its slot in the top-N.

**Resolved variables** (`query::resolved_variables`; `--resolve-vars` on
`get`, `dump` and `styles --values`, plus the matching tool arguments): a
read-time join that emits a `resolved_variables` array alongside the raw
data, never mutating it. Each entry is `{pointer, variable_id,
variable_name, values_by_mode}` when the variables table has that id
(mode ids mapped to mode names through the collection), or `{pointer,
variable_id, source: "unresolved"}` when it doesn't. The flag never
errors; it resolves what it can. On `styles --values`, the annotation
covers bindings whose pointer falls under that style type's raw-JSON
prefix (FILL ⇒ `/fills`, and so on).

## 14. Vector geometry and mirror config

`pull --geometry` adds `?geometry=paths` to the Tier-1 file fetch
(`api::file_url`), so vector nodes carry `fillGeometry` and
`strokeGeometry` in their `raw`.

The flag is stored per mirror as the single `Rec::MirrorConfig` row (§3,
§4) rather than as a new field on `FileMeta`, because postcard struct
fields are positional: widening an existing `Rec` struct breaks every
store already on disk, while appending an enum variant does not.

`store::effective_geometry(flag, stored)` is `flag || stored`, so every
later pull of that mirror keeps requesting geometry: the watch tick,
`figmog_sync`, `figmog_open` and an auto-open inside `resolve` all read
the stored flag first, and the records never churn from flag drift. The
way back off is `pull --fresh`, whose callers pass `stored = false`
without reading the store they are about to wipe. `figmog_open` takes an
optional `geometry` argument; omitting it preserves whatever the mirror
already has. The config row is written in its own write transaction,
outside `sync`'s, and is never swept.

## 15. Images

`figmog images <node-id>... [--format png|svg] [--scale N] [--out <dir>]`
and the `figmog_images { ids, format?, scale?, file? }` tool fetch image
bytes. Nothing in figmog calls them on its own: the images endpoints are
Figma's most rate-limited, so a fetch happens only when a person or an
agent asks for one.

**Two fetch kinds behind one command.** Renders come from `GET
/v1/images/:key?ids=…&format=…&scale=…`, which returns per-node URLs that
are then downloaded. Fills come from the `fills` of the requested nodes:
every `imageRef` hash found there is resolved through `GET
/v1/files/:key/images` (fetched at most once per invocation) and
downloaded. One invocation makes at most one call of each kind.

**Cache.** Bytes land in `images` as `Rec::ImageBlob` rows keyed by a
hash of kind, subject, format and scale (§3). A hit requires the stored
row's identity fields to match the request and its `file_version` to
equal the mirror's current version, so a repeat request against an
unedited file spends nothing. Rows tagged with an older version are
evicted by the same version-change pass that evicts `proxy_cache` (§5).

**Explicit stores.** `figmog --db <store> images <node-id>...` exports
matching cached renders and fills without a token or file key. The store
does not record its Figma file key, and neither `.figmog/current`, the
store's path nor a node URL supplies that identity for an explicit store.
Each cache miss reports an actionable file-identity error in the manifest;
cache hits still write successfully. Fetching missing images requires
`figmog pull <file-key-or-url>` followed by `figmog images <node-id>...`
without `--db`.

`figmog serve --db <store> --no-watch` without a file positional supports
the same offline cache extraction through `figmog_images`. Matching renders
and fills retain their inline content alongside per-item missing-identity
errors for cache misses. Busy and not-ready sessions still reject image
requests before scanning the cache (§6).

**Output.** The CLI writes each item into `--out` (default
`./figmog-images/`) and prints a manifest of
`[{id|ref, kind, format, bytes, cached, path?, error?}]`, where `bytes`
is a count. Exit is 0 when at least one item was written, 1 otherwise,
and the manifest is the stdout payload either way. The MCP tool returns
the same manifest plus, per item within the 1 MiB inline cap: an `image`
content block (base64) for raster formats, or a `text` block carrying the
raw markup for `format=svg`. SVG ships as text because agents consume the
markup directly and clients render `image/svg+xml` blocks inconsistently.
The cap is measured on the *encoded* length, since base64 inflates raw
bytes by about a third; an item over the cap becomes a text entry naming
the `figmog images --out` command that downloads it.

**Rate limits.** A 429 is recorded per item in the manifest (an `error`
field carrying the Retry-After) and the rest of the results still come
back: exit 0 if at least one item succeeded, else 1.

**Routing.** `figmog images` both spends API budget and writes cache
rows, so unlike a read command it cannot open a store `serve` holds. With
the socket reachable it routes through serve's own `figmog_images` tool
(serve owns the caching); with `--no-socket` or no server listening it
fetches and writes directly, like `pull`. `--db` always bypasses the socket
and opens the explicit store, including for cached-image extraction.
