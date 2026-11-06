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
                                      │ 18 materialized sinks (frozen names)
                                      ▼
                         ┌─────────────────────────┐
                         │  query.rs                 │  one source of truth for
                         │  status/pages/tree/...    │  every read answer
                         └───────┬─────────┬─────────┘
                                 │         │
                    ┌────────────┘         └────────────┐
                    ▼                                    ▼
         ┌─────────────────────┐              ┌─────────────────────────┐
         │  cli/ (ReadCommand)  │              │  dispatch.rs              │
         │  pretty-JSON stdout, │              │  ReadCommand (17 reads) +  │
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
Image requests (NWS-2955) run on the same per-file workers and desktop
proxy calls on a dedicated desktop worker, so neither delays dispatch.

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
| `ImagePin(String)` | `ImagePinRec` | version-pin marker for one cached render row (§15) |
| `VariableSnapshot` | `VariableSnapshotRec` | one row: provenance and code names of the stored variable snapshot (§9) |

`Id`/`Rec` are **append-only enums** — postcard encodes variant indices
positionally, so inserting a new variant anywhere but the end would
silently corrupt every existing on-disk store. `VariableSnapshot` is the
most recent addition and sits last in both enums (`ImagePin` immediately
before it); the next new record kind goes after `VariableSnapshot`, never
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

`ImagePinRec` (§15): `generation: u32` and `file_version`, keyed by the
same image key as the render row it marks. A render row is a hit only
with a marker whose `generation` equals `images::RENDER_PIN_GENERATION`
(currently 2, meaning version-pinned requests with same-version snapshot
invalidation) and whose `file_version` equals the row's. It is a separate record kind
rather than a new `ImageBlobRec` field for the same reason as
`MirrorConfig`. Generation-1 pins are treated as misses because those
builds could retain stale images across same-version edits. Fill rows
carry no marker.

`VariableSnapshotRec` (§9): `source` (`"file"` for `import-variables`,
`"enterprise_rest"` for a pull's `variables/local` fetch), `format`
(detected export shape: `"rest"` for `{meta: …}`, `"bare"` for the
plugin-console object), `format_version` and `exported_at` (the export's
top-level `formatVersion`/`exportedAt`, `None` when absent — no current
exporter emits them), `digest` (`fnv1a64:<hex>` over the raw file bytes,
or over a pulled response's canonical JSON — a change detector, not an
integrity proof), and `variables`: one `VariableCodeRec { id,
code_syntax, hidden_from_publishing, remote }` per variable, sorted by
id, with `code_syntax` as sorted `(platform, name)` pairs from
`codeSyntax`. It is written in the same transaction as the snapshot it
describes. A separate record kind rather than new `VariableRec` fields
for the same reason as `MirrorConfig`.

Older builds can open these stores but do not maintain `variable_snapshot`
or `image_pins`. Their writes can leave provenance from an earlier import
or orphan pin markers. Markers without matching image rows are ignored
and swept on a version-changing pull. A variable-id mismatch reports
`legacy` with null provenance; changes to existing ids or collections
alone cannot be detected. Re-import or an Enterprise pull refreshes the
snapshot; avoid writing to the same store with mixed build versions.

**Determinism contract:** every map-shaped field is a sorted `Vec` of
pairs; every canonical-JSON string comes from `serde_json` without
`preserve_order` (`serde_json::Value` is `BTreeMap`-backed here, so
`to_string` is already sorted-key deterministic); two flattens of
byte-identical file JSON must produce byte-identical postcard-encoded
records, because `KeyedStream::upsert` uses byte equality as its change
detector. No `HashMap` iteration ever reaches an output boundary.

## 4. Pipeline and sinks

`store::figmog_pipeline!()` wires the flattened records into 18
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
| `image_pins` | Table | image cache key hash | render version-pin markers (§15) |
| `variable_snapshot` | Table | `0u8` (one row, like `meta`) | variable snapshot state and provenance (§9) |

`mirror_config`, `images`, `image_pins` and `variable_snapshot` are
appended at the pipeline root's tail, so the reader tuple grows at the
end and older stores keep decoding (a store created before `image_pins`
or `variable_snapshot` existed opens with that sink empty). All four sit
outside `collect_sweepable` (§5): a file sync never removes a config or
snapshot row, and image rows are evicted by version instead.

`bound_to`'s edges are deliberately not deduped before feeding the
inverted index: `InvertedIndex` is set-semantic, so duplicate `(node,
var)` edges (a variable bound at multiple property paths on the same
node) collapse harmlessly to the same membership.

## 5. Sync, churn, and sweep

`store::sync` applies one flattened file in a single write transaction:
upsert every record and the meta row, then remove ids that were live
before this sync but are absent from it now (the sweep). It returns a
`Churn { added, changed, removed, unchanged }` count.
The variable snapshot row contributes no added/unchanged count, but a
change to an existing snapshot's per-variable code metadata contributes
one `changed`; changes to provenance alone contribute nothing (§9).

**What's sweepable:** nodes, components, component sets, and styles
always are (`collect_sweepable`, read via `rtx` *before* `sync`, since
this is a single-writer process — no write races). Variables and
variable collections are swept only when the caller opts them in for
that cycle via `collect_variable_ids` — i.e. only on a pull that actually
fetched an Enterprise `variables/local` export this time. On a
non-Enterprise or `--from-file` pull, stored variables sit entirely
outside `sync`'s live/sweep accounting: manually imported records persist
across ordinary pulls until the next import replaces them (§9). The meta
row is never sweepable. `import-variables` uses the same
`collect_variable_ids` sweep set in its own transaction
(`store::replace_variable_snapshot`).

**Idempotence:** re-syncing byte-identical records is a no-op through the
whole pipeline — `unchanged` increments, no index writes happen. A
spurious watch trigger or a repeated `pull` therefore costs exactly one
Tier-1 fetch and nothing else downstream.

**Cache eviction** is selected separately from the file-record sweep and
does not contribute to churn. Direct, explicit-sync and watch pulls use
`store::sync_with_cache_eviction` to apply the file and evict cache rows
in the same write transaction. If the version moved, `stale_cache_ids`
and `stale_image_ids` select mismatched proxy/image rows and orphan pins.
If the version stayed the same but metadata or records changed, all
images and pins are invalidated. Identical snapshots skip both cache
scans. Interruption rolls back the snapshot and eviction together, so a
retry cannot mistake a partially updated mirror for an unchanged one.
`pull --fresh` also removes caches through its whole-store wipe.

**Tree validation:** the shared flattener rejects document roots that are
not objects with a non-empty string `id` and `type: "DOCUMENT"`. Every
descendant must also be an object with non-empty string `id` and `type`
fields; unknown descendant types remain supported. A node's `children`,
when present, must be an array of valid nodes. Missing `children` and empty
arrays are accepted, including on an empty DOCUMENT root. Invalid trees
return an error with the node's location, before an ordinary CLI import,
fresh replacement, or server pull can sync a partial tree or sweep existing
subtrees. Other fields remain unknown-field-preserving.

**Deep trees:** saved-file and REST file imports support 500 nested frames
(and are not subject to serde_json's default 128-container parsing limit).
Node walks, variable-binding scans, and tree/subtree reconstruction use
explicit work stacks. File responses and emitted trees are drained
iteratively on cleanup; JSON parsing and printing use `serde_stacker`.
Deserialization guards partial arrays/maps and replaced duplicate-key values,
so malformed deep input returns an error instead of overflowing during cleanup.
Stored raw JSON and component definitions use this same guarded parser across
variable inference, styles, image-fill scanning, and inventory reads. `where`
moves matched values and compares containers iteratively; pending results and
subtree frames retain guards until response handoff, including error paths.

**Dependency justification:** `serde_stacker` is needed because disabling
serde_json's recursion limit alone exposes the CLI and the server's ordinary
worker stacks to overflow during deserialization and serialization. Its
adapters grow the stack while retaining serde_json's validation and sorted
object semantics; it avoids introducing a separate JSON parser or changing
the persisted record schema. `serde_json/unbounded_depth` enables these
adapters to accept full Figma trees beyond the default limit. The regression
fixtures are generated synthetically as JSON text.

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
after the whole fetch → flatten → atomic sync and cache eviction → geometry
persistence cycle succeeds. During every stage, existing readers see the
previous complete mirror, including for the file being pulled. Other files
have independent workers. No on-disk records or sink names change.
An unchanged version, modification timestamp and record set skip both
stale proxy-cache and image-cache scans, avoiding decoding cached image
blobs. Initial or version-changing pulls scan both tables and evict
mismatched rows before publication. When the version stays the same but
the modification timestamp or stored records change, all image rows and
pins are invalidated; Figma can reuse a version ID across live edits.

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
`ready: false`, null metadata and zero nodes. Status always includes the
`variables` summary (`state: "none"` without stored variable state).
Data reads return a clear not-ready error pointing at these tools, except
`figmog_vars` can read stored imports (§9 "Import-only stores"); `ready`
stays false until a pull succeeds.

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
A socket timeout/disconnect does not cancel accepted file-worker work.
Queued reads and desktop calls are skipped once their requester disconnects;
frames still waiting on the control plane cannot open or sync a mirror.
Stdin EOF or a broken stdout pipe shuts down the server using the bounded
read/desktop/writer drain described in §6a. Running file-worker commands
finish before their stores close; control-plane follow-ups are discarded. Forced process termination may interrupt work; fold's
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
Stdout writes are manual (`writeln!` + flush) on a stdout writer thread
(NWS-2962), so a vanished stdio reader causes clean shutdown instead of a
panic.

**Slow clients and heavy reads (NWS-2962):** the loop never writes to a
client and never runs a whole-mirror scan.

- *Heavy reads run on read workers.* Every local read except the point
  lookups `figmog_pages` and `figmog_path` (bounded by page count and node
  depth) — i.e. `figmog_node`, `figmog_components`, `figmog_tree`, `figmog_subtree`,
  `figmog_find`, `figmog_search`, `figmog_instances`, `figmog_styles`,
  `figmog_uses`, `figmog_vars`, `figmog_stats`, `figmog_text`,
  `figmog_where`, `figmog_at`, plus `figmog_status` and `figmog_files`
  (their node count scans the mirror) — runs on a pool of
  `min(4, available_parallelism)` read workers. Classification is by
  measured cost on a synthetic 100k-node mirror: the inline lookups take
  under 0.2 ms. Node reads always run on workers because a single raw
  payload or its direct child list can be arbitrarily large; `figmog_components` is O(sets × components) — 0.34 s on
  a 2k-set × 20k-component library (`measure_components_cost`); status ~90 ms, stats ~0.5 s, a whole-document subtree
  ~0.4 s (≈20 MB), vars ~0.3 s, tree/where ~0.2–0.4 s, the rest 25–150 ms
  (`measure_read_costs`, an ignored test). Routing, the not-ready check
  and status's `ready`/`pull` lifecycle (as of the request) are decided on
  the loop; the worker reads a leased published view (no store lock),
  then frames and serializes the response itself, so even a 20 MB answer
  costs the loop one queue push. Responses are byte-identical to the
  inline ones; they are delivered like other deferred responses (≤ 2 ms
  polling while reads are outstanding) and may arrive out of request
  order on one stream (match by id). Jobs run in arrival order.
- *Per-requester bound.* A connection (or stdio) may have at most 32
  calls pending (worker reads, images, desktop calls); a further
  `tools/call` from it is refused at once with `too many pending
  requests on this connection (limit 32); …` (socket `-32000`, stdio
  `isError`).
- *Queue and connection bounds.* At most 128 reads wait in the shared read
  queue, plus the running workers. At most 128 desktop calls wait behind
  its active operation. Each file retains at most 128 queued image requests,
  regardless of requester connection turnover. A full queue answers with a
  busy tool error. The socket admits at most 64 connections, including
  writers still flushing after their readers close; excess connections are closed.
- *Dropped connections cost nothing more.* A read or desktop call whose
  connection has closed or been dropped is not started (if queued) or
  accepted. An already-running upstream operation cannot be cancelled.
- *Reset barrier (per store).* A store close or reset (1) validates its
  input, (2) stops admitting reads for that store only (its data reads,
  inline or worker, are refused with "this mirror's store is being reset;
  retry shortly"; `figmog_files` lists it with null metadata; its worker
  starts no new pull, poll or image request), (3) waits — polled each loop
  iteration, never blocking other files — until every admitted worker read
  has released its view and its worker is idle, then (4) closes, resets
  and reopens the store and (5) publishes the new session in place. A
  store is never deleted or reopened underneath a live view. Shutdown
  applies steps 2–3 (bounded by the exit drain) before closing stores; a
  worker read still running at the exit deadline is detached with its own
  lease rather than awaited, so shutdown stays within the deadline and no
  store is reset or reopened under it: the leased fold readers retain
  fjall's own keyspace lock even after `.figmog-lock` is released. A pull
  acknowledged while reset is pending retains its job id and runs after
  reopening. No
  tool resets a store yet; `SessionManager::begin_reset` /
  `service_resets` are the mechanism NWS-2963 builds on (its only
  operation today is a close-and-reopen, which clears completed job history).

**Off-loop images and desktop calls (NWS-2955):** `figmog_images` and
proxied desktop calls are answered later instead of on the loop, so a slow
image download (up to the two-minute HTTP budget per request) or desktop
call (10-second timeout) never delays other requests. `figmog_images` runs
on its file's worker, which owns the cache writes; desktop calls run one at
a time, in arrival order, on a single desktop worker that owns the upstream
connection (results stay verbatim and uncached, §8); the desktop
re-detection checks (§8) run on the same worker, with a due recovery check
ahead of queued calls. A check may issue several HTTP requests, each
with its own timeout; no aggregate 10-second probe budget is promised. The loop parks each
deferred call with its requester (stdio, or the socket connection id) and
its JSON-RPC id, drains a completion channel every iteration (at most 20ms
latency), and writes the response to that requester, framed exactly as an
immediate one. Responses may therefore arrive out of request order on one
stream, which JSON-RPC permits (match by id); the CLI's socket client sends
one request per connection. Routing never uses the JSON-RPC id, so
connections may reuse ids. A response for a connection that has closed is
dropped. A worker that stops before answering still answers with an error.
When a file's worker is occupied by a pull or metadata poll,
`figmog_images` returns a prompt busy error (including for cache hits),
rather than waiting behind that operation. While it is busy with another
image request, a new one queues behind it (FIFO), up to 128 queued requests
per file. Further requests receive a prompt busy error. Accepted image
work remains queued after its requester disconnects. A watch poll that came
due meanwhile, or a due watch retry, gets the worker before the next queued
image request (outside the global two-second dispatch floor, at most once
per due poll), so continuous image traffic cannot starve watch; queued
image requests wait for that poll. Under `--no-watch` there is no watch
poll or retry to dispatch here, so queued image requests simply run in
order. If a pull takes the worker instead,
queued image requests get the busy error. If a file's worker thread is
gone, its requests fail with an error rather than crashing the loop. A
desktop write's expedited polls (§8) are recorded when its result is
delivered.

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
`upstream: "connected" | "unreachable" | "disabled"` (current as of the
request, §8 "Re-detection"), spliced in at the call site that knows it (never inside `query::status` itself), and the
variable snapshot summary under `variables` (§9).
`figmog_open` also takes an optional `geometry` boolean (§14).

The 19 per-file tools each carry the optional `file` property described
above; `figmog_open` and `figmog_files` don't.

**Argument types (NWS-2944):** local tool arguments are type-checked
whether required or optional. A missing required field is `missing
required field: <name>`; a present field of the wrong JSON type is
``expected <type> for `<name>`, got <json type>`` (e.g. ``expected
integer for `depth`, got string``), and a negative integer argument
(`depth`, `limit`) is ``expected non-negative integer for `<name>`, got
<n>``. An omitted optional argument or an explicit JSON `null` keeps the
default (`figmog_where`'s `equals` is a value, not an option: `null`
there still means equality to null). These are ordinary tool errors —
`isError` on stdio, the socket's `-32000` error (§6a), and `figmog
call`'s stderr error. Desktop tools' arguments are forwarded unchecked.
Local `arguments` must be an object or `null` (the latter means no
arguments). Read arguments are validated before opening or pulling a
mirror. Floating-point representations of `depth` and search `limit`
remain rejected with `got floating-point number <value>`; point lookup's
`limit` retains its non-negative integral-number rule (§10). CLI point
coordinates must be finite so direct and socket serialization agree.

**Read adapters (NWS-2902):** the 17 local reads (every `figmog_*` tool
except `figmog_sync`, `figmog_images`, `figmog_open` and `figmog_files`)
share one typed representation, `dispatch::ReadCommand`. Tool JSON (MCP,
the socket, `figmog call`) is parsed into it with the argument typing
above; the clap read commands convert into it directly; the CLI's socket
client serializes it back to tool JSON. Each read has exactly one query
arm (`ReadCommand::execute`). Defaults live in the typed payload: an
omitted or `null` optional is absent/false, search's `limit` defaults to
10 on both the CLI (`-n`) and the tool, and `where`'s `equals` is absent
only when omitted. Routing (`file`, node-URL inference) and status
transport metadata (`upstream`, serve's `ready`/`pull`) stay with each
adapter. Synthetic parity tests run every read through direct CLI,
`figmog call`, socket CLI and MCP and require identical JSON.

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
Sending `initialize` is also how a connection opts in to notifications
(`notifications/tools/list_changed`, §8); stdio gets them once its client
has sent `initialize`, as every MCP client does. The CLI never sends
`initialize`, so it gets none, and its reader still skips any frame that
carries a `method` and matches its reply by JSON-RPC id. A notification
is queued on the connection's writer like a response, so it is ordered
after every response already queued.
An acceptor thread gives each connection its own reader
thread, which tags frames with a connection id and forwards them into the
same `mpsc` channel stdin feeds; the connection's write half is
registered, with its own writer thread, before its reader starts, so a
response always has somewhere to route. Routing is by connection id and
deferred token, never by JSON-RPC id.

**Output (NWS-2962):** the loop only queues response frames; each
connection's writer thread writes them in order. The queue is bounded at
32 frames and 64 MiB (queued plus in flight; one larger frame is still
accepted onto an empty queue), and each response has one overall 20 s
write deadline from its first byte — the remaining time is re-applied
before every 64 KiB write call, so a client that trickle-reads cannot
renew it. A full queue or a missed deadline drops the connection: its
socket is shut down, queued frames discarded, and its pending reads
and desktop calls skipped. A client that closes its side has frames already queued flushed
and later responses dropped. A writer closes its write half after flushing,
so a half-closed client receives EOF without waiting for more registry
activity; a vanished client is a silent no-op. Stdio
has one client (the agent), so its writer thread has an unbounded queue
and no drop policy (dropping it would be fatal); a broken stdout shuts
serve down as before. Socket traffic interleaves with MCP frames and
watch ticks like any other request; per-file writes stay on each file's
worker. Image, desktop and worker-read responses are written when their
off-loop work finishes (§6), to the connection that asked.

**Shutdown with deferred work:** on stdin EOF (or the request channel
closing) serve stops accepting requests and starts no new work, but still
writes the responses it owes, to stdio and to still-open sockets, for up to
15 seconds, so `printf
'<request>\n' | figmog serve` gets its image or desktop response. Image
requests still queued (not started) are answered with a shutting-down
error, and the desktop worker stops taking queued calls (and skips queued
desktop checks) at once: its
in-flight call still finishes and is answered, while each queued one is
answered with a shutting-down error without being sent upstream (it may be
a write). Responses not ready by then are dropped. Worker reads still owed
are answered within the same window; writer threads then get what is
left of it to flush queued frames. A running image request
still finishes before exit with its file's worker, like a running pull; the
desktop worker is detached. Each upstream HTTP request has a 10-second
timeout, but a probe can need several requests and outlive this drain.
Queued desktop calls receive their shutdown errors immediately, even when
a probe is blocked. Socket writers still unfinished at the deadline are
aborted; dropping a connection mid-response may leave a partial JSON frame
followed by EOF.

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
running with Dev Mode MCP enabled. While it is connected, `tools/list`
merges figmog's 21 local tools with every upstream tool verbatim
(description prefixed `"[via Figma desktop] "`); while it is missing,
local-only tools. A failed startup probe prints one stderr line.

**Re-detection (NWS-2959).** `serve` keeps checking after startup, so
opening the desktop app after the agent started, or quitting it, needs no
restart. Every check runs on the desktop worker (§6), never on the loop,
with a 2 s connect timeout:

- while the desktop is missing, a full handshake every 30 s;
- while it is connected, an MCP `ping` and refreshed `tools/list` on the
  current session every 60 s, falling back to a fresh handshake if it fails (a restarted
  desktop app does not know the old session);
- immediately (at most once a second) when a desktop call fails below
  the tool level (transport or protocol error), and when a `tools/call`
  names a non-`figmog_*` tool that is not in the registry while the
  desktop is missing. Recovery runs ahead of queued calls; calls wait for
  the one-second minimum probe gap rather than using a known stale session.

The desktop counts as gone only when a check fails: one failed or timed
out call just triggers a check, and a tool-level `isError` result never
does. A call to a desktop tool while it is missing is still refused at
once with `unknown tool: <name>`; it is not held for the check it
triggers. When a check changes the exposed upstream tool list, the
registry is re-merged under the namespace rule below and every client that
sent `initialize` gets `{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}`
(`initialize` advertises `capabilities.tools.listChanged: true` when the
desktop worker is enabled, and `false` under `--no-upstream`). Desktop
tools use a canonical order based only on their client-visible definitions.
Unsupported metadata cannot change that order. Notifications compare the merged,
client-visible registry: reordering, changes to filtered collisions, or
unsupported upstream fields send nothing. Dynamic tool names and
descriptions are owned by the current registry and released when replaced.
A check
that confirms the current state, or a reconnect with the same tools, sends
nothing. `figmog_status`'s `upstream` follows the current state
(`connected` / `unreachable`). `--no-upstream` starts no desktop worker,
never probes, and reports `disabled`. Forwarding change notifications
through a second figmog server is deferred to NWS-2953.

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
   `--from-file` pulls never call it (no network involved). A `Some`
   pull also writes the `VariableSnapshot` row (`source:
   "enterprise_rest"`) in the same sync transaction, but — like the meta
   row — `store::sync` excludes it from added/unchanged counts.
   Comparing an existing snapshot's per-variable code names and flags
   against the new ones adds one to `changed` when they differ, so
   code-name-only edits are visible to token consumers. Provenance-only
   changes (source, format, export time or digest) do not count. An
   identical Enterprise re-pull reports zero churn, and the first
   Enterprise pull onto a store that already holds those variables
   without a snapshot row reports `added: 0`.
2. **Mirrored bindings + inference (always on).** Every node's
   `bound_variables` plus Figma's own baked-in resolved value are scanned
   at read time (`vars::infer_from_nodes`) into per-variable usage
   (binding sites) and observed values. This covers each variable's
   default-mode value only; a non-default mode's value is visible only
   where a frame explicitly overrides that mode.
3. **Manual import (optional).** `figmog import-variables <export.json>`
   (`vars::parse_variables_snapshot`) writes the same full-fidelity
   records auto-sync would, from either the Enterprise REST shape or the
   bare object a Figma plugin-console export produces (any plan, via the
   Plugin API run from Figma's own developer console). **An import is one
   atomic snapshot that replaces the previous one:** in a single write
   transaction it upserts every record and the `VariableSnapshot` row
   (`source: "file"`) and removes every stored variable and collection
   absent from the new export — whether it came from an earlier import,
   an Enterprise pull, or a pre-snapshot store. It prints `{"imported":
   <variables>, "removed": <variables swept>}`. Collections absent from
   the export are swept too, but neither count includes collections.
   A later pull with no Enterprise export of its own never sweeps imported
   records; they persist until the next import, an Enterprise pull, or
   `pull --fresh` (which wipes the whole store, imports included).

   **Invalid files are rejected whole.** The file is read, parsed and
   validated (top-level `variables` and `variableCollections` objects,
   non-empty variable names and resolved types, collection names and
   defaults, valid collection/mode references, and correctly typed code
   names and flags — `vars::check_import_entries`, which only
   `import-variables` applies; an Enterprise pull's parse stays lenient
   per entry, so one malformed `variables/local` entry never fails a
   pull) before the store is opened; an unreadable,
   non-JSON or wrongly shaped file exits 1 with `invalid variables file
   …` and leaves the previous snapshot — records and provenance —
   untouched. A rejected attempt is not persisted, so "invalid" is the
   import command's error, never a stored state.

   Replacing stored variables with an export containing zero variables
   requires `--allow-empty`. `--dry-run` validates the file and reports
   `{imported, removed, removed_ids}` with sorted removed variable ids
   without changing records or provenance; it can preview an empty
   replacement without `--allow-empty`.
   Omitted `codeSyntax` becomes an empty map; omitted
   `hiddenFromPublishing` and `remote` retain the parser's `false` defaults.
   Duplicate JSON object keys follow JSON parsing's last-value-wins rule;
   exporters must produce unique ids.

**Snapshot states** (`query::variable_snapshot`), reported as
`variables` in `figmog status`/`figmog_status` and as `snapshot` by
`figmog vars --snapshot`/`figmog_vars {snapshot: true}`:

| `state` | meaning |
|---|---|
| `none` | no snapshot row and no variables or collections stored |
| `legacy` | variables or collections stored without a matching snapshot row (missing provenance or mismatched variable ids); provenance fields are null |
| `empty` | a snapshot row whose export held no variables |
| `current` | a snapshot row with at least one variable |

Each summary also carries `source`, `format`, `format_version`,
`exported_at`, `digest` (null without a snapshot row) and the stored
`variables`/`collections` counts.

**Import-only stores.** `figmog status`/`figmog_status` on a store that
was never pulled (no `meta` row) but holds variable state (snapshot
state other than `none` — e.g. `import-variables --db <new path>`)
answers rather than erroring: `name`, `version`, `last_modified` and
`synced_at_unix_ms` are null, `nodes` is the stored node count (0), and
`variables` carries the snapshot summary as usual. Only a store with no
`meta` row and state `none` keeps the "no mirror here" error. Through
`figmog serve` the same summary appears in `figmog_status` before the
first successful pull (with `ready: false`). `figmog_vars`, including
`{snapshot: true}`, and routed `figmog vars` can read these stored imports.
Before any pull attempt, socket-routed plain `figmog status` prints the
same output as direct `figmog status --db`. Once a first pull is queued,
running or failed, plain routed status exits 1 with the not-ready error
and includes the pull failure when available; `figmog call figmog_status`
retains the full lifecycle details.

`figmog vars`/`figmog_vars` prefer an authoritative record (auto-synced
or imported) over inference whenever one exists for a given variable id.
Authoritative rows also carry `code_syntax` (platform → name object),
`hidden_from_publishing` and `remote` from the snapshot row — null for a
legacy snapshot. Without `--snapshot`/`snapshot: true` the result stays
the plain row array; with it, `{snapshot, variables}`.

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
dispatch, `run`, the shared store-opening/JSON-writing helpers, and the
mapping of the 17 read-only query commands onto `dispatch::ReadCommand`),
`src/cli/pull.rs` (`pull`, the typed `PullError`, `.figmog/current`),
`src/cli/call.rs`
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

**Point lookup.** `figmog at --page <page-id-or-url> --x X --y Y`
(and `figmog_at` with required `page`, `x`, `y`) searches one CANVAS's
coordinate space. `page` must identify an existing CANVAS; a missing,
unknown, or non-page reference is an error. Page references accept bare
ids, hyphenated ids, and Figma page URLs. `--under <node-id-or-url>`
intersects the page with that subtree, including its root; an unknown
scope is an error. Hidden nodes and descendants of hidden nodes are
excluded. Bounds use inclusive left/top and exclusive right/bottom
edges. Matches retain the array shape `[{id, name, type, page_id, area}]`,
sorted by area ascending then id, and are truncated after filtering and
sorting. `--limit N` (MCP `limit`) defaults to 100; zero returns an empty
array. Hit testing traverses the requested page's children with cycle
protection; no store schema change is needed.

**Socket-first routing (§6a).** When neither `--no-socket` nor `--db` is
given, every read command, plus `tools`, `call` and `images`, first tries
`<.figmog>/serve.sock`. Reachable ⇒ the command becomes a client of the
running `serve`: `cli::read_command` maps the subcommand to its typed
`dispatch::ReadCommand`, whose `to_tool_call` produces the tool name and
argument object serve parses back into the same command, so the
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
node URL in `id`, `under`, `target`, or `page`, then `.figmog/current`.
An explicit `--db` takes precedence
over all of these. Current is injected as a routed `file` argument only
for per-file local tools, when no explicit non-null `file` was supplied
and no node URL selects a file. `figmog_open` and `figmog_files` keep
their management semantics: `file` names the file to open for the former,
and the latter lists all sessions without a routing target. A routed
`figmog_status` result drops the server-only `upstream`, `ready` and `pull`
fields, keeping ordinary CLI output byte-identical for an existing mirror.
Before a first successful mirror, plain `figmog status` returns a clear not-ready error
(unless the store holds imported variable state and no pull has been attempted — §9 "Import-only stores");
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
and one of `id`/`under`/`target`/`page` is a URL that names a file,
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
`--under <id>` on `text`/`find`/`search`/`where`/`at` and the matching tool
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
/v1/images/:key?ids=…&format=…&scale=…&version=…`, which returns per-node
URLs that are then downloaded. `version` is the mirror's current
`FileMeta.version` (percent-encoded), read once before the request and
reused as the cached row's `file_version`, so a pull that lands mid-request
can't relabel it. **This does not freeze live edits.** A live check against
an owned test file found different node contents and modification times
under the same returned version ID. A cache miss can therefore reflect
later upstream edits, and a node deleted upstream may no longer render
even while it remains in the mirror. A rejected pinned request
fails without an unpinned retry; HTTP 400/404 errors identify the file and
mirrored version and advise re-pulling. Saved exports must belong to the
selected file and reference a version Figma can still render. The
request parameter follows Figma's [image endpoint documentation](https://developers.figma.com/docs/rest-api/file-endpoints/#get-image);
an immutable historical-render guarantee remains unresolved (NWS-2997).
Fills come from the `fills` of the requested nodes:
every `imageRef` hash found there is resolved through `GET
/v1/files/:key/images` (fetched at most once per invocation) and
downloaded. One invocation makes at most one call of each kind.
If the mirror has no non-empty version, renders are requested unpinned
and stored without a pin marker, so they remain cache misses on later
requests.

**Cache.** Bytes land in `images` as `Rec::ImageBlob` rows keyed by a
hash of kind, subject, format and scale (§3). A hit requires the stored
row's identity fields to match the request and its `file_version` to
equal the mirror's current version. Pulls that change metadata or records
without changing the version invalidate all cached images and pins, so
repeated requests against an unchanged mirror spend nothing. A render hit
additionally requires its `image_pins` marker (§3), written in the same
transaction as the row.
Renders saved by builds that requested them without `version` have no
marker — their bytes may show a newer design than their label — so they
read as misses. With a file identity and credentials they are refetched
and overwritten, spending API budget again on that first request.
Without either, they return a distinct upgrade error and retain the
existing bytes. For a file-keyed mirror without credentials, the error
explains how to restore the token, restart any running `figmog serve`,
and retry the image request. When using `FIGMA_TOKEN`, the restarted
server must receive the updated environment. An
explicit offline store instead needs a file-keyed mirror to refresh them.
Fill rows need no marker: fills are addressed by content hash, and the fills endpoint
takes no version. Rows tagged with an older version, and their markers,
are evicted by the same version-change pass that evicts `proxy_cache`
(§5).

**Explicit stores.** `figmog --db <store> images <node-id>...` exports
matching cached fills and version-verified cached renders without a
token or file key. The store does not record its Figma file key, and
neither `.figmog/current`, the
store's path nor a node URL supplies that identity for an explicit store.
Each cache miss reports an actionable file-identity error in the manifest;
cache hits still write successfully. Fetching missing images requires
`figmog pull <file-key-or-url>` followed by `figmog images <node-id>...`
without `--db`.

Pre-upgrade render bytes without a valid pin remain on disk but cannot be
verified against the mirrored version. They produce a distinct upgrade
error rather than a cache hit; offline extraction of those renders with
the new build requires first refreshing them through a file-keyed mirror.

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
