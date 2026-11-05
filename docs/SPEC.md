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
                                      │ 20 materialized sinks (frozen names)
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
         │  pretty-JSON stdout, │              │  ReadCommand (18 reads) +  │
         │  or a socket client  │              │  tool_registry (22 tools)  │
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
| `RenderIndex(String)` | `RenderIndexRec` | fingerprint-indexed render cache entry (§15, opt-in) |
| `RenderBlob(String)` | `RenderBlobRec` | immutable render bytes, keyed by their SHA-256 (§15, opt-in) |

`Id`/`Rec` are **append-only enums** — postcard encodes variant indices
positionally, so inserting a new variant anywhere but the end would
silently corrupt every existing on-disk store. `RenderIndex` and
`RenderBlob` (NWS-2958) are the most recent additions and sit last in both
enums, in that order (`VariableSnapshot` immediately before them); the
next new record kind goes after `RenderBlob`, never between existing
variants. The same rule is why a new setting arrives as
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
(currently 1, meaning "requested with `version=<file_version>`") and
whose `file_version` equals the row's. It is a separate record kind
rather than a new `ImageBlobRec` field for the same reason as
`MirrorConfig`. Fill rows carry no marker.

`RenderIndexRec` (§15, NWS-2958): `fingerprint` (= the `Id::RenderIndex`
key), `scheme` (`images::RENDER_FINGERPRINT_SCHEME`), `subject` (node
id), `format`, `scale_milli`, `file_version` (the version the render
request was pinned to — provenance only, never compared on a hit), `seq`
(store-local insertion order, for oldest-first eviction), `sha256` (the
`Id::RenderBlob` key of its bytes) and `byte_len`. `RenderBlobRec`:
`sha256` (= the key) and `bytes`. The bytes sit in their own record so
the orphan and quota scans decode only the small index rows. Both are new
record kinds rather than `ImageBlobRec` fields, for the same reason as
`MirrorConfig`, and neither is ever written unless a caller opts in to
render reuse.

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

**Determinism contract:** every map-shaped field is a sorted `Vec` of
pairs; every canonical-JSON string comes from `serde_json` without
`preserve_order` (`serde_json::Value` is `BTreeMap`-backed here, so
`to_string` is already sorted-key deterministic); two flattens of
byte-identical file JSON must produce byte-identical postcard-encoded
records, because `KeyedStream::upsert` uses byte equality as its change
detector. No `HashMap` iteration ever reaches an output boundary.

## 4. Pipeline and sinks

`store::figmog_pipeline!()` wires the flattened records into 20
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
| `render_index` | Table | render fingerprint | opt-in cross-version render reuse (§15) |
| `render_blobs` | Table | SHA-256 hex of the bytes | immutable render bytes for `render_index` (§15) |

`mirror_config`, `images`, `image_pins`, `variable_snapshot`,
`render_index` and `render_blobs` are appended at the pipeline root's
tail, so the reader tuple grows at the end and older stores keep decoding
(a store created before one of them existed opens with that sink empty).
All six sit outside `collect_sweepable` (§5): a file sync never removes a
config or snapshot row, image rows are evicted by version instead, and
render-reuse rows only by the render cache's own orphan cleanup and byte
quota (§15).

`bound_to`'s edges are deliberately not deduped before feeding the
inverted index: `InvertedIndex` is set-semantic, so duplicate `(node,
var)` edges (a variable bound at multiple property paths on the same
node) collapse harmlessly to the same membership.

### Maintenance keyspace and post-write compaction (NWS-2941)

fold opens fjall with its default configuration and never flushes a
memtable itself, so everything a large pull writes stays in fjall's
journal, and every later open of the store replays the whole journal
before it can answer anything. On a synthetic 100,001-node mirror that
is a 93 MB journal and ~1.33 s for even `figmog path 0:1`.

figmog therefore compacts a store after large writes (`src/compact.rs`),
with fold's handle closed and the store's `.figmog-lock` ownership
anchor (§5, fresh replacement) held for the whole pass. It reopens the
same directory with fjall's own API — the default builder, exactly as
fold opens it — and:

1. flushes every keyspace's memtable into tables
   (`rotate_memtable_and_wait` on each of `list_keyspace_names()`);
2. commits one tiny constant row (`compacted` → `1`) to the
   **`figmog_maintenance`** keyspace. After a reopen fjall's
   append-mode journal writer reports stream position 0 until something
   is written, and fjall seals the active journal only during a flush
   that finds that position past 64,000,000 bytes;
3. flushes that keyspace: fjall seals the old journal and, every
   keyspace having persisted past it, deletes it (figmog waits on
   `journal_count()` for the deletion);
4. persists (`PersistMode::SyncAll`) and closes.

`figmog_maintenance` is not a pipeline sink — fold names those
`sink_<name>` and never opens anything else — so the frozen sink table
above is unchanged, and a binary without compaction opens a compacted
store as ordinary fjall data (it just never looks at that keyspace).
Outputs are byte-identical with and without compaction.

**When.** A journal smaller than fjall's 64 MB sealing threshold can't be
retired this way and is left alone (checked first from the journal files'
allocated size, without opening the store, then again after fjall's
recovery truncates its preallocated tail); its replay costs in proportion.
Otherwise compaction runs:

- after a CLI `pull` (direct, including `figmog call figmog_sync`) that
  added, changed or removed records, or was `--fresh` — never after a
  zero-churn re-pull;
- after a CLI `import-variables` that changed the snapshot;
- at `figmog serve` shutdown, per store, after the close barrier
  (`close_reads`) and after clients have their answers: each session is
  closed (its worker joined, its published view dropped) while its
  ownership anchor is kept, then compacted. A store that a detached read
  still holds a view of keeps fjall's own lock, so compaction's open fails
  and that store is left untouched (one stderr line). Serve never compacts
  under live views: not after an individual write job, and not during a
  reset-barrier reopen (which releases the store unchanged).

Compaction is best effort over already-committed data: a failure prints
`figmog: store compaction skipped: …` on stderr and never fails the
command that wrote. `FIGMOG_NO_COMPACT=1` (any value but empty or `0`)
disables it.

**Crash safety.** Every step is an ordinary fjall operation with fjall's
own crash safety (table writes and version swaps, journal sealing and
deletion after the data is persisted). A process killed at any point
leaves a store fjall recovers normally — at worst with the old journal
still in place, replayed as before — and the next compaction finishes the
job. `compact::tests::a_crash_during_compaction_leaves_a_readable_store`
aborts a child process after each step and re-reads every row.

**Measured** (synthetic 100,001-node file, `figmog pull --from-file`,
best of 3 cold CLI processes, 4-core Linux; `cargo test --test compact --
--ignored --nocapture`):

| command | journal replayed | compacted |
|---|---|---|
| journal on disk | 93.2 MB | 0 B |
| `path 0:1` | 1.328 s | 0.004 s |
| `get 500:3` | 1.327 s | 0.004 s |
| `status` | 1.347 s | 0.084 s |
| `search "copy number 7"` | 1.334 s | 0.076 s |
| `vars` | 1.478 s | 0.220 s |

The pull itself pays one more replay plus the flush (6.5 s → 8.7 s here).
An unchanged re-pull of the compacted store is still zero churn.

**Pin bumps.** `rotate_memtable_and_wait` (`Keyspace`) and the
`SingleWriterTxKeyspace::inner()` accessor are `#[doc(hidden)]` in fjall
3.1.9 — public but undocumented — and the 64 MB sealing rule is
fjall-internal behavior. fjall is pinned `=3.1.9`; `AGENTS.md`'s
pin-bump checklist requires re-verifying both on any fjall or fold bump
(`compact::tests::journal_shrinks_after_compaction` fails if the journal
stops being retired).

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
across ordinary pulls until the next import replaces them (§9). The meta
row is never sweepable. `import-variables` uses the same
`collect_variable_ids` sweep set in its own transaction
(`store::replace_variable_snapshot`).

**Idempotence:** re-syncing byte-identical records is a no-op through the
whole pipeline — `unchanged` increments, no index writes happen. A
spurious watch trigger or a repeated `pull` therefore costs exactly one
Tier-1 fetch and nothing else downstream.

**Version-keyed cache eviction** is a separate pass, not folded into
`sync`'s sweep (its churn accounting stays untouched by this feature):
after a sync whose file *version* actually moved, every `proxy_cache` row
(`store::stale_cache_ids`) and every `images` row
(`store::stale_image_ids`, which also yields each row's `image_pins`
marker) tagged with the old version is removed through
the one `store::evict_stale_cache` function. Fingerprint-indexed renders
(`render_index`/`render_blobs`, §15) are not version-tagged and never
touched by this pass. This runs after every
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

By default `figmog serve` is a stdio relay to the repository's shared
background owner (§6b), and this section describes that owner's engine.
`--db`, `--figmog-root` and `--no-socket` run the same engine in the
`figmog serve` process itself, with stdio as one more client.

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
not-ready error pointing at these tools rather than an empty answer. If
the store already holds an imported variable snapshot (§9 "Import-only
stores"), status also carries its `variables` summary; `ready` stays
false until a pull succeeds.

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
Stdout writes are manual (`writeln!` + flush) on a stdout writer thread
(NWS-2962), so a vanished stdio reader causes clean shutdown instead of a
panic.

**Slow clients and heavy reads (NWS-2962):** the loop never writes to a
client and never runs a whole-mirror scan.

- *Heavy reads run on read workers.* Every local read except the point
  lookups `figmog_pages`, `figmog_node` and `figmog_path` (bounded by the
  page count, one node with its direct children and bindings, and its
  depth) — i.e. `figmog_components`, `figmog_tree`, `figmog_subtree`,
  `figmog_find`, `figmog_search`, `figmog_instances`, `figmog_styles`,
  `figmog_uses`, `figmog_vars`, `figmog_stats`, `figmog_text`,
  `figmog_where`, `figmog_at`, `figmog_spec` (it joins a subtree with
  the whole variables read, §17), plus `figmog_status` and `figmog_files`
  — runs on a pool of `min(4, available_parallelism)` read workers.
  Classification is by measured cost on a synthetic 100k-node mirror: the
  inline three take under 0.2 ms; `figmog_components` is linear in sets +
  components (NWS-2942) — 65 ms on a 2k-set × 20k-component library
  (`measure_components_cost`); a whole-document subtree ~0.4 s (≈20 MB),
  stats ~0.2 s, vars ~0.1 s, tree/where ~0.2–0.4 s, the rest 25–150 ms
  (`measure_read_costs`, an ignored test). Status and files are O(1) in
  the mirror's size: each published view carries its node count in
  memory (from the pull that published it, or counted once by the
  session worker when an existing mirror is opened), so a status poll
  never scans the `nodes` table. Direct CLI `figmog status` still counts
  the table (one-shot). Routing, the not-ready check
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
- *Dropped connections cost nothing more.* A read whose connection has
  closed or been dropped is not started (if queued) or not accepted.
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
  store is reset or reopened under it. The operations behind it are a
  close-and-reopen (`StoreReset::Reopen`, tests only) and a routed `pull
  --fresh` (`StoreReset::Fresh`, below): step 1 is the worker's fetch,
  parse and flatten, and step 4 clears the store's contents (its ownership
  anchor stays) under the same ownership before reopening it empty.
  Either starts fresh job history; CLI writes still queued for the store
  carry over to the reopened session.

**CLI writers through serve (NWS-2963):** when its socket answers, `figmog
pull` (plain, `--geometry`, `--from-file`, `--fresh`) and `figmog
import-variables` run as *operations* of serve instead of opening the
store (§10). They are internal socket-only tools — `figmog_internal_pull
{file, fresh?, geometry?, from_file?}`, `figmog_internal_import_variables
{file, content, name}` and the receipt read `figmog_internal_operation
{id}` — never listed by `tools/list` and refused on stdio as unknown, so
the agent tool surface is unchanged (agents keep `figmog_open`/`figmog_sync`).

- *Operation ids.* Each accepted write gets id `<generation>-<seq>`: the
  generation is this process's pid and start time (hex), the sequence
  counts from 1. The requester's connection first receives
  `{"method": "notifications/figmog/operation", "params": {"id": ...}}`;
  the `tools/call` is a deferred response (NWS-2955) sent only once the
  write committed **and** its view was published — `{"operation": id,
  "result": <churn | {"imported","removed"}>}` — or a `-32000` error if it
  failed, in which case nothing changed. The wait is the requester's own
  connection; the loop never blocks on it.
- *Receipts.* A finished operation's outcome is kept for 10 minutes;
  `figmog_internal_operation {id}` (any connection) returns `{"id",
  "state": "succeeded", "result"}`, `{"id", "state": "failed", "error"}`,
  `{"id", "state": "running"}`, or an unknown-operation error (another
  process's id, or expired). Receipts and ids are in memory only.
- *Ordering and coalescing.* Writes run on the file's worker (still its
  only writer) in arrival order, ahead of queued watch/sync jobs, and,
  like a direct pull, without waiting out a failed pull's backoff. Two
  writes are never merged, except identical plain pulls (network, not
  fresh, same `geometry`) that have not started yet *and are adjacent in
  the queue* — a plain pull joins only the last queued write, never one
  with another write (an import, a saved-file, fresh or differently
  configured pull) queued after it: they share one fetch and apply, each
  keeping its own operation id. A network write uses
  serve's credentials. While a write is pending on a not-yet-mirrored
  store, reads queue no implicit first pull. A successful routed pull
  updates `last_success` (`"job_id": null`, plus `"operation"`); it is not
  a lifecycle job.
- *Validation first.* `from_file` (a parsed `GET /v1/files/:key`
  response) and `content` (an export's exact text; the snapshot digest is
  over it) are checked — parse, flatten, variable entries — before any
  store change, exactly like the direct path. A fresh pull's fetch and
  flatten run on the worker before its reset barrier closes.
- *Owner loss.* A write whose worker stops before reporting is answered
  `outcome unknown: ...`; if the connection ends first, the CLI reports
  `outcome unknown` with the announced operation id (exit 1) and never
  retries or falls back to a direct open.
- *Shutdown.* When serve shuts down (stdin EOF, or a background owner's
  loop exiting), a write already running still finishes within the
  15-second drain (§6a "Shutdown with deferred work"): its session keeps
  processing completions — a fresh pull's reset and apply included — so
  it commits, publishes and is answered as usual. A write still queued is
  not started: it is answered JSON-RPC error `-32001` `not accepted: ...
  reconnect and retry` even though its operation id was announced, and
  the CLI treats it like any "not accepted" answer (nothing was applied).
  Only the shared background owner's refusal is retried on a new
  connection (a new owner may take over, §16); an in-process serve's
  refusal is not sent again to that exiting process — the CLI reports
  `figmog serve shut down before accepting this write; nothing was
  applied — rerun the command` (exit 1). A reconnect that fails after a
  refusal reports the same "nothing was applied", never `outcome unknown`.
- *Store owner records.* A serve writes `figmog serve (pid N, socket
  <path>)` (or `no socket`) into each store lock (`.figmog-lock`) it
  takes; every other opener clears it. A command finding a store held
  (`--db`, `--no-socket`) names that owner in its `store is locked by ...`
  error.

**Off-loop images and desktop calls (NWS-2955):** `figmog_images` and
proxied desktop calls are answered later instead of on the loop, so a slow
image download (up to the two-minute HTTP budget per request) or desktop
call (10-second timeout) never delays other requests. `figmog_images` runs
on its file's worker, which owns the cache writes; desktop calls run one at
a time, in arrival order, on a single desktop worker that owns the upstream
connection (results stay verbatim and uncached, §8); the desktop
re-detection checks (§8) run on the same worker, between calls. The loop parks each
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
image request, a new one queues behind it (FIFO). A watch poll that came
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
to a one-day maximum; `0` is accepted and means "as fast as the floors
allow". With no sessions the idle watch wakeup is also floored at 2 s, so
an empty `--interval 0` server never busy-waits. On `Tick::Changed`, completion queues a pull through
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

**The 22 `figmog_*` tools** (`dispatch::tool_registry`): 13 core reads
(`figmog_status`, `figmog_pages`, `figmog_tree`, `figmog_node`,
`figmog_subtree`, `figmog_find`, `figmog_search`, `figmog_instances`,
`figmog_components`, `figmog_styles`, `figmog_uses`, `figmog_vars`,
`figmog_sync`) + 5 whole-file structural queries (`figmog_stats`,
`figmog_path`, `figmog_text`, `figmog_where`, `figmog_at`) +
`figmog_spec` (the code-ready spec, §17) + `figmog_images` (§15) + 2 multi-file management tools that aren't
per-file (`figmog_open`, `figmog_files`). Every tool but
`figmog_sync`/`figmog_open`/`figmog_images` reads the local mirror at
zero Figma API cost; `figmog_sync` and `figmog_open` schedule pulls, and `figmog_images` fetches renders and fills, so
those three are the local tools that spend Figma's rate budget.
`figmog_status`'s result also carries
`upstream: "connected" | "unreachable" | "disabled"` (current as of the
request, §8 "Re-detection"), spliced in at the call site that knows it (never inside `query::status` itself), and the
variable snapshot summary under `variables` (§9).
`figmog_open` also takes an optional `geometry` boolean (§14).

The 20 per-file tools each carry the optional `file` property described
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

**Read adapters (NWS-2902):** the 18 local reads (every `figmog_*` tool
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

The background owner's socket, election and handshake are §6b. An
in-process serve (`--figmog-root`, or `--db` with a socket) binds
`<figmog-root>/serve.sock` — `--db` serves use the
working-directory-relative `.figmog/` — (mode 0600, inside a root
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
loop reads. An incoming frame may be at most 256 MiB (raised from 8 MiB
for NWS-2963: a routed `pull --from-file` sends the saved file response
in its request); a longer line without a newline drops the connection,
and the CLI refuses up front to send a larger write, pointing at stopping
serve and applying it directly. Output is bounded separately (below). `initialize` is optional for socket clients: `tools/call` is
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
skipped. A client that closes its side has frames already queued flushed
and later responses dropped; a vanished client is a silent no-op. Stdio
has one client (the agent), so its writer thread has an unbounded queue
and no drop policy (dropping it would be fatal); a broken stdout shuts
serve down as before. Socket traffic interleaves with MCP frames and
watch ticks like any other request; per-file writes stay on each file's
worker. Image, desktop and worker-read responses are written when their
off-loop work finishes (§6), to the connection that asked.

**Shutdown with deferred work:** on stdin EOF (or the request channel
closing) serve stops accepting requests and starts no new work, but still
writes the responses it owes, to stdio and to still-open sockets, for up to
15 seconds (covering the desktop's 10-second timeout), so `printf
'<request>\n' | figmog serve` gets its image or desktop response. Image
requests still queued (not started) are answered with a shutting-down
error, CLI writes still queued with "not accepted" (§6 "CLI writers through
serve"), and no pull or watch poll is started from then on — neither a
queued watch/sync pull nor one a metadata poll still in flight asks for
when it completes `Changed`; running work —
including a routed write and its fresh-pull reset — keeps being serviced
(no watch polls) until it publishes and answers. The desktop worker stops taking queued calls (and skips queued
desktop checks) at once: its
in-flight call still finishes and is answered, while each queued one is
answered with a shutting-down error without being sent upstream (it may be
a write). Responses not ready by then are dropped. Worker reads still owed
are answered within the same window; writer threads then get what is
left of it to flush queued frames. A running image request
still finishes before exit with its file's worker, like a running pull; the
desktop worker is detached, its in-flight call bounded by the upstream
timeout.

Socket `tools/call` registry and handler failures (including upstream
protocol/transport failures) use a top-level JSON-RPC `error` with code
`-32000`, so the CLI emits its standard stderr error and exits 1.
Genuine upstream results remain verbatim `result` values, including
tool-level `isError: true`; those are successful forwarding operations.
Stdio retains MCP `isError` content for registry and handler failures.

The CLI side of the plane is in §10.

## 6b. One background owner per repository (NWS-2953)

Source: `src/owner.rs` (election lock, socket address, discovery,
handshake, launch), `src/serve/background.rs` (the owner's loop side),
`src/serve/relay.rs` (the stdio relay).

**Shape.** Every `figmog serve` without `--db`, `--figmog-root` or
`--no-socket` is a thin relay between its agent's stdio and the
repository's *owner*: one detached `figmog serve --background` process per
mirror root (§16) that opens the mirrors, polls and pulls them (§6), and
owns the desktop probe (§8). The first relay launches it; every later relay
and CLI command connects to it. Closing an agent's session only closes that
relay's connection. Relays never open a store, poll, pull or probe the
desktop.

**Project identity** is the canonical mirror root (`<git-common-dir>/figmog`
inside Git, `<project>/.figmog` outside), the same from every worktree.

**Election.** `<mirror root>/owner.lock` is a permanent lock file (0600,
never deleted), opened close-on-exec. An owner takes an exclusive `flock`
on it (`File::try_lock`) before anything else and holds it across stale
socket removal, bind, readiness (discovery), serving and shutdown; it is
released only after the stores are closed and compacted and the socket and
discovery record are removed (or by the kernel when the process dies,
including a crash during startup). A launched owner that cannot take it
exits 0 at once without touching anything: another process is or is about
to be the owner. Relays may launch concurrently (each launches at most one
owner at a time, again only after its launched one exited with the owner
still unreachable); exactly one wins, and every relay connects to it. A
lock file that disappears or is replaced makes its holder shut down (see
"Idle shutdown" below), since another process could lock the new file.

**Socket.** `/tmp/figmog-<uid>/<16 hex>.sock`, the hex a 64-bit FNV-1a hash
of the project identity: under 40 bytes, so always within `sun_path`
(104 bytes on macOS), and independent of `$TMPDIR` and
`$XDG_RUNTIME_DIR`. The directory is created 0700 and checked like a
mirror directory (§10: owned by the user, no replaceable ancestors; `/tmp`
is sticky), so other OS users can neither connect nor pre-plant it; the
socket is 0600. Under the lock, a file already at the address is a
leftover of this project's previous owner: it is removed only if a connect
to it fails and it is a socket owned by the user (a live listener or a
foreign file is an error). On exit the socket is unlinked only if it is
still the inode this owner bound. An owner whose socket disappears or is
replaced (checked every second) shuts down rather than run unreachable.

**Discovery.** Once it can answer, the owner writes `<mirror
root>/owner.json` (0600; temp file, fsync, rename):
`{"generation","pid","project","protocol","socket","version"}`. The
generation is the owner's pid and start time (hex), also the prefix of its
operation ids (§6). Clients connect to the socket it names, so every
process finds the same endpoint whatever its environment. The owner
removes the record at exit if it is still its own.

**Handshake.** The first frame on every owner connection is
`{"method": "figmog/hello", "id": "figmog-hello", "params": {"client":
"relay"|"cli", "project", "version", "protocol", "settings"?, "attach"?}}`.
The owner refuses a different project, binary version or protocol version
(currently 1) with error `-32002` and an explanation; the client checks the
owner's answer `{"attach_errors", "generation", "pid", "project",
"protocol", "settings", "version"}` the same way. Either way the relay (and
the CLI) stops with that explanation — "let it exit (it stops on its own
after it has had no clients for a while), or stop it now with `kill <pid>`"
— and never starts a second owner beside it. A relay connection is then
answered like stdio: tool failures as MCP `isError` content and no
internal tools (`figmog_internal_*` are unknown tools there); a CLI
connection keeps §6a's JSON-RPC errors and internal write tools. `attach`
lists the relay's startup files: the owner opens each, queueing a first
pull for a missing mirror when it watches (the old `figmog serve FILE...`
startup); open failures come back in `attach_errors` and fail the relay's
startup.

**Worktree context stays in the relay.** The owner has no default file and
writes no `.figmog/current`. For every per-file local tool call with no
explicit `file` and no node URL naming one, the relay injects its own file:
the first startup `FILE`, else `figmog.toml`'s default (else
`.figmog/current`), else the single file this relay has itself referenced;
with none it answers the "no file specified" error itself. A file whose
mirror in this worktree is still in the old per-checkout location (§16) is
refused with a pointer at `figmog migrate` (and a startup file fails the
relay's start), never shadowed by a new shared mirror; the CLI likewise
never routes such a mirror to the owner (an in-process serve's local socket
still answers it, else it is opened directly). A file whose mirror does
not resolve at all (two mirrors, an interrupted `figmog migrate`) is
routed to no socket: the CLI reports the same error as with `--no-socket`,
even while an in-process serve holds one of the two. Nor does the CLI route a
per-file call to the owner when the invoking worktree resolves no file
(no explicit `file`, node URL or project default): it reports that
worktree's "no mirror here" rather than another worktree's file. The owner
itself never treats its one open session as a default. Desktop and
management calls pass through unchanged. Every request a relay forwards
— any frame with an `id` that is not a `notifications/` method, including
one whose method is empty, missing or not a string (the owner answers it
"method not found") — goes to the owner under a relay-assigned id, mapped back to the agent's own
id on the answer, so no agent id is ever mistaken for the relay's own
(its replayed `initialize`).

**Settings.** The owner runs with the watch, `--interval` and
`--upstream`/`--no-upstream` settings of the relay that launched it, and
with that relay's environment (credentials included). A later relay asking
for different settings prints one stderr note per setting (`figmog: note:
interval 30s was not applied — ... runs with interval 10s ...`); nothing
changes until the owner restarts. Credentials therefore apply after the
owner's idle restart (§10 "Credentials").

**Idle shutdown.** The owner exits by itself once it has had no open
connection, no unanswered deferred call and no pending CLI write for its
idle timeout (10 minutes; `FIGMOG_OWNER_IDLE_MS` overrides it, for tests).
Relays hold their connection for the agent's whole session, so an owner
idles out only when no agent is attached. Shutdown first *closes*: every
new request on any connection, the handshake included, is answered with
JSON-RPC error `-32001` `not accepted: ... reconnect and retry`, and nothing
is started for it; accepted work (deferred replies, writes) finishes; then
the loop exits and §6a's shutdown runs (drain, store close, compaction per
§4 "post-write compaction"), the socket and discovery record are removed,
and only then is the lock released. A connection accepted after the loop
exited is closed unanswered; clients treat that like "not accepted".

**Reconnects.** Only a request answered "not accepted" is ever sent again:
the relay reconnects — launching a new owner if none is left — and re-sends
it (at most 5 times); the CLI reconnects and retries (reads and writes
alike: nothing was started), waiting up to 60 s for the closing owner to go
away, then falls back to a direct open if no new owner appeared — except a
write, which then reports "nothing was applied" (§6 "CLI writers through
serve"). A write
the owner accepted is never retried: NWS-2963's `outcome unknown` stands.
If the owner connection drops with requests unanswered (the owner crashed
or was killed), the relay answers each with an `outcome unknown` tool error
and reconnects at once. On every new connection the relay repeats the
handshake (with its startup files) and the agent's `initialize` (under a
relay-internal id whose answer it swallows), so notifications keep
flowing.

**Notifications.** Notifications from the owner — `tools/list_changed`
from desktop re-detection (§8) — are forwarded verbatim to the relay's
agent once the agent has sent `initialize` (which the relay forwarded,
subscribing its connection).

**Launch.** The relay runs its own executable as `figmog serve
--background <settings>` with `setsid` (its own session: no terminal or
process-group signal of the agent's reaches it), stdin and stdout on
`/dev/null`, stderr appended to `<mirror root>/owner.log` (truncated at
launch past 1 MiB), working directory the relay's project root. A thread
reaps it, so it is never left a zombie. A launched owner that exits
non-zero before becoming ready fails the relay with the log's tail; the
relay waits at most 60 s for an owner (an old one's shutdown drain is
bounded at 15 s).

**Cost of an idle restart.** The watch watermark is not persisted (§6
"Watch watermarks"), so a restarted owner makes one reconciliation pull
per watched file at its first poll; no record kind was added to avoid it.
The acceptance test measures relay spawn → owner launched → first answer
at about 30 ms, and relay spawn → new owner → first read of an existing
mirror at 40–65 ms (debug build, Linux, synthetic fixture).

**Acceptance (tests/owner.rs).** Three worktrees (`git worktree add`), each
with its own relay, share one owner and read one mirror concurrently
through relays and the CLI with no lock errors, and one edit on a
synthetic Figma API causes exactly one pull. Also tested: 10 concurrent
relays → one owner (3 trials), 10 concurrent owner launches → one survivor,
a SIGKILLed owner (lock released, stale socket replaced), a relay surviving
its owner's crash, clients and relays arriving around the idle deadline
(12 trials), a version-mismatched owner refused by relay and CLI, two
repositories' independent owners, per-worktree default files, settings
notes, and a mirror awaiting migration.

The synthetic API is reached through `FIGMOG_API_BASE`, honored by serve's
sessions in debug builds only (release builds always use
`https://api.figma.com`, so no environment variable can send a token
elsewhere).

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
merges figmog's 22 local tools with every upstream tool verbatim
(description prefixed `"[via Figma desktop] "`); while it is missing,
local-only tools. A failed startup probe prints one stderr line.

**Re-detection (NWS-2959).** `serve` keeps checking after startup, so
opening the desktop app after the agent started, or quitting it, needs no
restart. Every check runs on the desktop worker (§6), never on the loop,
with a 2 s connect timeout:

- while the desktop is missing, a full handshake every 30 s;
- while it is connected, an MCP `ping` on the current session every 60 s,
  falling back to a fresh handshake if the ping fails (a restarted
  desktop app does not know the old session);
- immediately (at most once a second) when a desktop call fails below
  the tool level (transport or protocol error), and when a `tools/call`
  names a non-`figmog_*` tool that is not in the registry while the
  desktop is missing.

The desktop counts as gone only when a check fails: one failed or timed
out call just triggers a check, and a tool-level `isError` result never
does. A call to a desktop tool while it is missing is still refused at
once with `unknown tool: <name>`; it is not held for the check it
triggers. When a check changes the exposed upstream tool list, the
registry is re-merged under the namespace rule below and every client that
sent `initialize` gets `{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}`
(`initialize` advertises `capabilities.tools.listChanged: true`). A check
that confirms the current state, or a reconnect with the same tools, sends
nothing. `figmog_status`'s `upstream` follows the current state
(`connected` / `unreachable`). `--no-upstream` starts no desktop worker,
never probes, and reports `disabled`. Relaying serves do not forward
these notifications yet (NWS-2953).

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
   row — it is provenance, not a record: `store::sync` excludes it from
   churn, so `pull`, `figmog_sync` and serve's `last_success.churn` count
   only nodes/components/sets/styles/variables/collections, exactly as
   before the row existed. An identical Enterprise re-pull reports zero
   churn, and the first Enterprise pull onto a store that already holds
   those variables without a snapshot row reports `added: 0`.
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
   <variables>, "removed": <variables + collections swept>}`. A later
   pull with no Enterprise export of its own never sweeps imported
   records; they persist until the next import, an Enterprise pull, or
   `pull --fresh` (which wipes the whole store, imports included).

   **Invalid files are rejected whole.** The file is read, parsed and
   validated (top-level `variables` and `variableCollections` objects,
   every entry an object — `vars::check_import_entries`, which only
   `import-variables` applies; an Enterprise pull's parse stays lenient
   per entry, so one malformed `variables/local` entry never fails a
   pull) before the store is opened; an unreadable,
   non-JSON or wrongly shaped file exits 1 with `invalid variables file
   …` and leaves the previous snapshot — records and provenance —
   untouched. A rejected attempt is not persisted, so "invalid" is the
   import command's error, never a stored state.

**Snapshot states** (`query::variable_snapshot`), reported as
`variables` in `figmog status`/`figmog_status` and as `snapshot` by
`figmog vars --snapshot`/`figmog_vars {snapshot: true}`:

| `state` | meaning |
|---|---|
| `none` | no snapshot row and no variables or collections stored |
| `legacy` | variables or collections stored without a snapshot row (written before provenance existed); provenance fields are null |
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
first successful pull (with `ready: false`), and socket-routed plain
`figmog status` prints the same output as direct `figmog status --db`.

`figmog vars`/`figmog_vars` prefer an authoritative record (auto-synced
or imported) over inference whenever one exists for a given variable id.
Authoritative rows also carry `code_syntax` (platform → name object),
`hidden_from_publishing` and `remote` from the snapshot row — null for a
legacy snapshot — plus the variable's `scopes` and its collection's
`default_mode` (by name, the mode id when unnamed; NWS-2967). Without
`--snapshot`/`snapshot: true` the result stays the plain row array; with
it, `{snapshot, variables}`.

### Token export, diff and check (NWS-2967, NWS-2948)

`figmog tokens export [--out <path> | --stdout] [--force] [--format tailwind|css]
[--mode <collection>/<mode>=<selector>]...` renders the variable snapshot
as one CSS file; `figmog tokens diff [--file <path>]` and `figmog tokens
check [--file <path>]` (same `--format`/`--mode`) compare a committed file
with that rendering. The renderer and comparison are pure
(`src/tokens.rs`); the CLI half is `src/cli/tokens.rs`. DTCG JSON and
TypeScript output are out of v1 scope.

**How it reads.** None is a new read tool: the CLI maps each to the
existing `figmog_vars {snapshot: true}` read (`ReadCommand::Vars`), so it
is socket-routed to a running serve/background owner or opens the store
directly exactly like `figmog vars --snapshot` (§10 "Socket-first
routing"), and renders that JSON locally. A serving binary older than the
`default_mode` field is reported as an error asking to restart it.
Authoritative `vars` rows also carry `collection_id` (additive JSON).

**Settings.** In a project, the token file is `[stack] tokens`, the format
`[tokens] format` and the mode mapping `[tokens.modes]` (§16);
`--format` replaces the format and each `--mode` one mapping. With `--db`
there is no project: defaults apply, `export` writes to stdout unless
`--out` names a file, and `diff`/`check` need `--file`.

**Snapshot.** Only state `current` exports; `none`, `legacy` (no
provenance) and `empty` are exit-1 errors. Only authoritative rows are
exported — an inferred-only variable (bound in the file, absent from the
snapshot) is skipped and reported.

**Identity and names.** A token is its variable id, within its
collection, never its display name. Its CSS name is `codeSyntax.WEB`
(`--x` or `var(--x)`) when `x` is nonempty ASCII letters, digits, `-` and
`_` — WEB code syntax can be any expression, so anything else (a JS
identifier, `var(--x, red)`, `calc(...)`) is reported and falls back to
the name path: the variable name lower-cased, every run of
non-alphanumerics (including the `/` group separators) one `-`, prefixed
`--` (`Color/Brand Primary` → `--color-brand-primary`; no collection
prefix). A name with no ASCII alphanumerics is skipped and reported. Two
variables with one name — `brand/primary` and `brand-primary`, or
`primary` in two collections — are an error naming both, never an
overwrite: ``token name collision: `primary` (collection "Brand",
VariableID:…) and `primary` (collection "Product", VariableID:…) both
export as `--primary` …``.

**Modes.** Every collection's modes map to selectors: exactly one *root*
mode, written to `:root`, and every other mode its own selector. Each
collection's mapping comes from `[tokens.modes]`/`--mode` (keys
`"<collection>/<mode>"`; a key naming no mode of the snapshot, or more than
one, is an error), else the default:

- root: the mode mapped to `:root`; else the collection's one mode named
  like "light" (ASCII-case-insensitive substring) when it also has one
  named like "dark"; else its default mode. Several `:root` modes, or a
  default root mapped elsewhere, are errors;
- the one mode named like "dark" → `[data-theme="dark"]`;
- any other mode → `[data-<collection>="<mode>"]`, both kebab-cased like a
  name path (`Density`/`Compact` → `[data-density="compact"]`); a name
  with no ASCII alphanumerics must be mapped explicitly;
- `""` leaves a mode out (listed in the header as `(not exported)`).

Two modes of one collection on one selector are an error; modes of
different collections may share one (`"Color/Dark"` and `"Theme/Dark"`
both on `[data-theme="dark"]` switch together). Collections are
independent — theme × density × brand — and each block declares only its
own collections' variables, so nothing overwrites another collection.
Modes of one collection are not nested inside each other (other than
under the root mode).

**Output.** Deterministic, every block sorted by property name:

1. a header comment with the snapshot's `source`, `format`,
   `format_version`, `exported_at` and `digest` (null when absent), then
   one ` * theme: collection "<c>": mode "<root>" -> :root, mode "<m>" ->
   <selector>, …` line per collection with more than one mode (sorted by
   collection, then mode), or ` * theme: none (every collection exports one
   mode)`;
2. `:root { … }` — every exported variable's root-mode value;
3. one block per other selector (sorted), with each variable of a
   collection mapped there in that mode;
4. combined blocks `<own> <other>, <own><other>` (sorted; `<own>:is(<other>)`
   when `<other>` starts with a type selector) — see below;
5. format `tailwind` only: `@theme { … }` and `@theme inline { … }` — the
   Tailwind v4 theme (below). Empty blocks are omitted.

**Subtrees.** A custom property's `var()` resolves where the property is
declared, so a variable declared only in `:root` that aliases one a mode
overrides would keep its root value inside that mode's subtree. Each
mode's block therefore also redeclares every variable of *another*
collection that aliases, directly or through a chain (in any of its
modes), a variable the block declares — with its root value. For each of
that variable's own non-root modes, a combined block (its mode's selector
outside this one, or both on one element) declares it with that mode's
value; at specificity (0,2,0) it outranks both plain blocks, so nesting
either collection inside the other, or setting both on one element, keeps
both modes (`[data-theme="dark"] [data-density="compact"],
[data-theme="dark"][data-density="compact"] { --space-section: 32px }`).

**Values.** COLOR → `#rrggbb`, or `#rrggbbaa` below full alpha (each
channel `round(clamp(x, 0, 1) × 255)`). FLOAT units come from `scopes`,
each implying one unit: px for `GAP`, `WIDTH_HEIGHT`, `CORNER_RADIUS`,
`FONT_SIZE`, `LINE_HEIGHT`, `LETTER_SPACING`, `PARAGRAPH_SPACING`,
`PARAGRAPH_INDENT`, `STROKE_FLOAT` and `EFFECT_FLOAT`; `%` for `OPACITY`
(Figma opacity variables are 0–100: `50` → `50%`); none for
`FONT_WEIGHT`. When all scopes agree the FLOAT is `<n><unit>`; scopes
implying different units, `ALL_SCOPES`, no scopes or an unknown scope are
an *ambiguous unit*: unitless, reported. Numbers print in shortest
round-trip form (`16`, `0.5`). An alias → `var(--target)`; CSS resolves
chains. Every emitted FLOAT carries its own unit, so a FLOAT with a unit
aliasing a unitless one → `calc(var(--target) * 1px)` (or `* 1%`; a bare
`var()` would make `padding: 16` invalid, i.e. 0px), while a unitless
FLOAT aliasing one with a unit, or a px FLOAT aliasing a `%` one (and the
reverse), is skipped and reported (CSS cannot strip or convert a unit;
emitting the resolved number would silently detach the alias), as is an
alias between COLOR and FLOAT.
STRING, BOOLEAN and other types are skipped and reported, as is a
variable missing a needed mode's value.

**Aliases.** Aliases are kept as references, never flattened:

- an alias to a variable absent from the snapshot — typically a remote
  library variable the import doesn't carry — is reported as unresolved
  (`… not in the variable snapshot … unresolved, not flattened`) and the
  variable skipped;
- alias cycles are found per combination of modes: the alias graph over
  all modes is split into strongly connected components, and each that
  loops is checked under every combination CSS can produce — every set of
  matching mode selectors, each collection in the mode whose selector
  matches (its root mode when none does) — so collections sharing a
  selector (every dark mode on `[data-theme="dark"]` by default) switch
  together and a loop closing only when one is Light and the other Dark
  is no cycle; two modes of one collection never apply at once (a
  variable has one value per combination; more than 12 selectors ⇒ every
  member is skipped unchecked). A member of a cycle is skipped and
  reported with the cycle and either `in every mode` or `only in some
  modes (first: "Theme" mode "Dark")`; a loop no combination closes is no
  cycle;
- a variable aliasing a skipped one is skipped too, transitively.

**Tailwind namespaces.** COLOR → `--color-*`, spacing/size → `--spacing-*`,
radius → `--radius-*`; unitless FLOATs get no theme entry. A name already
in its namespace (`--color-brand`) is kept; otherwise the namespace is
prefixed (`--surface` → `--color-surface`). Entries:

- theme name ≠ token name ⇒ `@theme inline { --color-surface:
  var(--surface) }`, so the utility reads the token where it is used;
- theme name = token name and the value is the same alias in every
  emitted mode ⇒ `@theme inline { --color-action-primary:
  var(--color-brand) }` (the utility uses the alias target directly);
- otherwise ⇒ `@theme { --color-brand: <root value> }`: the utility reads
  `var(--color-brand)`, which the unlayered mode blocks override.

A theme name that collides with another theme name or another token's
name is an error naming both variables (format `tailwind` only).

**Export output and report.** To the token file (`--out <path>`, else
`[stack] tokens` in a project), creating its directory and leaving
identical bytes untouched. It never writes through a symlink (the token
file itself being one is an error, `--force` or not); `[stack] tokens`
must resolve inside the project (its nearest existing ancestor,
symlinks resolved, is under the project root); and an existing file that
does not start with ``/* Generated by `figmog tokens export` `` — a
hand-written stylesheet named as the token file — is an error unless
`--force`. Stdout gets `{out, status: written|unchanged,
format, tokens, theme, modes, report}` JSON. `theme` is the one collection
with a mode on `[data-theme="dark"]` as `{collection, light_mode,
dark_mode}` (else null; `figmog spec` shows it); `modes` lists every
collection's `{collection, mode, selector}` (`selector` null when left
out). With `--stdout`, `--out -`, or `--db` without `--out`, the CSS goes
to stdout and, when anything was reported, one `{"report": [...]}` JSON
line to stderr. Report entries are `{variable_id, name, reason}`, sorted.

**Diff.** `tokens diff` compares the committed file (`--file`, else
`[stack] tokens`) with a fresh rendering, as flat CSS: declarations are
matched by block (selector, whitespace-collapsed, or `@theme`/`@theme
inline`) and property name. It prints `{status, file, snapshot,
header_changed, added, removed, changed}` and exits 0: `status` is
`up_to_date` (byte-identical), `out_of_date` or `missing` (no file: every
declaration is `added`); `added`/`removed` entries are `{name, block,
value, variable_id}`, `changed` entries `{name, block, from, to,
variable_id}` (`variable_id` from the fresh export, null for a removed
name), sorted by name then block; `header_changed` flags a different
header comment (snapshot provenance or mode mapping); `snapshot` is the
provenance below. A file that isn't flat CSS blocks (nested blocks, text
outside a block) is an exit-1 error.

**Check.** `tokens check` passes only when the committed file is
byte-identical to the rendering, printing `{status: "ok", file,
certifies: "matches the last published snapshot", result: "<file> matches
the last published snapshot", snapshot: {source, format, format_version,
exported_at, digest}, note}` — the snapshot's provenance (NWS-2961). It
certifies the file against the variable snapshot in the mirror, never the
live Figma file, and `note` says so: Figma changes published after that
snapshot are seen only once it is re-imported or re-pulled. The
comparison is byte-exact, header included, so a re-import whose values
are identical but whose provenance (`exported_at`, digest) differs reports
the file stale (only `header_changed`) — intended: the file must match
the last published snapshot, provenance and all, and `tokens export`
refreshes it. Otherwise it
prints the diff report on stdout and exits 1 with the error "`<file>`
does not match the last published snapshot (N added, N removed, N
changed[, header … changed]) — run `figmog tokens export` to regenerate
it" (or "`<file>` does not exist — …"). It never writes the file.

**Fixtures.** `tests/fixtures/tokens/variables.json` (synthetic: a
Light/Dark `Theme` collection, an alias chain `action/hover →
action/primary → brand → ocean/500` and a GAP alias of a unitless FLOAT)
exports to the golden `export.css` (tailwind) and `export.plain.css`
(css); `modes.json` (Brand × Theme × Density with aliases across them, a
dark-only alias cycle and an alias to a remote variable, `Brand/Globex`
mapped to `.brand-globex`) to `modes.css` (`tests/tokens.rs`, which also
covers `diff`/`check` and the figmog.toml settings in a project;
`FIGMOG_BLESS=1` regenerates the goldens).
`packages/react/test/tokens.test.ts` compiles `export.css` and
`modes.css` with Tailwind v4 and asserts computed values in Chromium: both
modes, dark subtrees, and theme/brand/density modes nested either way or
on one element, through nested semantic aliases.

## 10. CLI surface

JSON is the CLI's only output mode (there is no `--json` flag — there's
nothing to select between). Every command prints pretty-printed JSON on
stdout via one seam (`cli::write_json`) — the one exception is `tokens
export` to stdout (`--stdout`, `--out -`, or `--db` without `--out`), whose
stdout is the CSS file itself (§9); every failure prints
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
mapping of the 18 read-only query commands onto `dispatch::ReadCommand`),
`src/cli/pull.rs` (`pull`, the typed `PullError`, `.figmog/current`),
`src/cli/call.rs`
(`tools`, `call`, `import-variables` — the desktop-proxy CLI parity
surface, so every tool `figmog serve` would expose is reachable without
an MCP client), `src/cli/images.rs` (`images`, §15),
`src/cli/socket.rs` (the client half of the control plane, §6a),
`src/cli/migrate.rs` (`migrate`, §16), `src/cli/init.rs` (`init` and
`settings`, §16), `src/cli/doctor.rs` (`doctor`, §16), and
`src/cli/tokens.rs` (`tokens export|diff|check`, §9 — mapped onto the
`figmog_vars` read, then rendered as CSS and compared with the token
file), and `src/cli/map.rs` (`map check|coverage`, §18 — `coverage` mapped
onto the `figmog_components` read, then compared with `figmog.map.json`),
and `src/cli/assets.rs` (`assets export`, §19 — the `figmog_subtree` and
`figmog_components` reads plus `figmog_images`, then files written into
the project).
`src/cli/auth.rs` handles credential commands and hidden terminal input;
`src/auth.rs` owns credential resolution and native storage.

`figmog tools` never opens the store (it only needs an optional upstream
probe), and `figmog auth` manages credentials independently. Neither
requires a resolved mirror — no established `.figmog/current` and no
`--db` needed. `figmog call` does need one, since local tool dispatch
reads the store.

Default mirror paths come from the project layout (§16): the key's
*active store* — `<mirror root>/<key>/db`, or an old
`<project>/.figmog/<key>/db` that hasn't been migrated yet. The default
file is `figmog.toml`'s `[files] default`, else `.figmog/current` (in the
project root). Every command except `version`, `auth` and `--db`
invocations discovers the project first, so a malformed `figmog.toml` or
`figmog.local.toml` fails every such command with `<file>:<line>: <message>`
(`doctor` reports it as its failed `config` check instead; `init`,
`doctor` and `settings` refuse `--db`).
`figmog migrate [FILE]` (§16) moves old mirrors into the shared root.
Persisted `.figmog/current`
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
on the next CLI invocation or, for agents, when the shared background
owner restarts after idling out (§6b); stop it sooner with `kill <pid>`
from `<mirror root>/owner.json`. Tokens remain subject to
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

**Socket-first routing (§6a, §6b).** When neither `--no-socket` nor `--db` is
given, every read command, plus `tools`, `call` and `images`, first tries
the repository's background owner (through `<mirror root>/owner.json` and
the handshake), then an in-process serve's `<.figmog>/serve.sock`.
Reachable ⇒ the command becomes a client of the
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
node URL in `id`, `under`, or `target`, then `.figmog/current`.
An explicit `--db` takes precedence
over all of these. Current is injected as a routed `file` argument only
for per-file local tools, when no explicit non-null `file` was supplied
and no node URL selects a file. `figmog_open` and `figmog_files` keep
their management semantics: `file` names the file to open for the former,
and the latter lists all sessions without a routing target. A routed
`figmog_status` result drops the server-only `upstream`, `ready` and `pull`
fields, keeping ordinary CLI output byte-identical for an existing mirror.
Before a first successful mirror, plain `figmog status` returns a clear not-ready error
(unless the store already holds an imported variable snapshot — §9 "Import-only stores");
`figmog call figmog_status` exposes the full lifecycle instead.

Desktop-proxy calls do not interpret `file` or node URLs as mirror
selectors (§8): direct mode opens the current mirror (or an explicit
`--db`), while serve forwards desktop calls without resolving a mirror.
Both paths forward uncached and preserve desktop arguments exactly,
including an absent `file` or explicit null, string, number, or object.
Socket routing never injects `.figmog/current` into desktop arguments.

**Writers (NWS-2963).** `pull` (including `--fresh`, `--from-file`,
`--geometry`) and `import-variables` resolve their mirror exactly as
direct mode does, then try the same socket. Reachable ⇒ they run as serve
operations (§6 "CLI writers through serve"): a `--from-file` response is
read and parsed in the invoking worktree (the direct path's `reading`/
`parsing` errors) and sent as content; an import is read and fully
validated locally (same errors, nothing sent for an invalid file) and its
exact text sent. The command returns only after the write committed and
was published, printing the same JSON as the direct path, byte for byte
(churn, or `{"imported","removed"}`); a successful routed pull also
writes `.figmog/current` and the `.gitignore` entry as a direct pull
does. Serve-side failures are the direct path's messages. If serve goes
away first: `outcome unknown` with the operation id, exit 1, no retry and
no direct fallback. Unreachable ⇒ direct. `--db` and `--no-socket` never
route. While `serve` holds a store, a direct open's lock error names it
(`store is locked by figmog serve (pid …, socket …) — …`); an unnamed
holder keeps the generic message, which for `pull` adds "or ask the
running serve: figmog call figmog_sync". With no running server, direct
`figmog call figmog_sync` remains synchronous and returns churn.

**Single-writer constraint:** fjall allows only one open handle per
store. `figmog serve` holds its `--db`'s lock for its whole life; a CLI
command that opens the same store directly (`--no-socket` or an explicit
`--db`) while `serve` runs gets a clean `store is locked by figmog serve
(pid …, socket …) — ...` error (exit 1; the generic `store is locked — is
figmog serve running? ...` when the holder left no owner record), never
fold's raw lock panic (exit 101) — see `open_store_checked`.

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

`figmog images <node-id>... [--format png|svg] [--scale N] [--out <dir>]
[--reuse-renders] [--refresh]` and the `figmog_images { ids, format?,
scale?, renders?, reuse?, refresh?, file? }` tool fetch image bytes. `renders: false` (default true; NWS-2952) skips the renders and
fetches only the fills the nodes' own `fills` reference — what `figmog
assets export` (§19) uses for image fills. Nothing in figmog calls them on its own: the images endpoints are
Figma's most rate-limited, so a fetch happens only when a person or an
agent asks for one.

**Two fetch kinds behind one command.** Renders come from `GET
/v1/images/:key?ids=…&format=…&scale=…&version=…`, which returns per-node
URLs that are then downloaded. `version` is always the mirror's current
`FileMeta.version` (percent-encoded), read once before the request and
reused as the cached row's `file_version`, so a render matches the
mirrored design even after the file is edited upstream, and a pull that
lands mid-request can't relabel it. A node deleted upstream but still in
the mirror renders from that version too. Fills come from the `fills` of the requested nodes:
every `imageRef` hash found there is resolved through `GET
/v1/files/:key/images` (fetched at most once per invocation) and
downloaded. One invocation makes at most one call of each kind.

**Cache.** Bytes land in `images` as `Rec::ImageBlob` rows keyed by a
hash of kind, subject, format and scale (§3). A hit requires the stored
row's identity fields to match the request and its `file_version` to
equal the mirror's current version, so a repeat request against an
unedited file spends nothing. A render hit additionally requires its
`image_pins` marker (§3), written in the same transaction as the row.
Renders saved by builds that requested them without `version` have no
marker — their bytes may show a newer design than their label — so they
read as misses and are refetched and overwritten. Fill rows need no
marker: fills are addressed by content hash, and the fills endpoint
takes no version. Rows tagged with an older version, and their markers,
are evicted by the same version-change pass that evicts `proxy_cache`
(§5).

**Render reuse across versions (NWS-2958, opt-in).** Per-version caching
throws every render away on any edit anywhere in the file. With
`--reuse-renders` (tool: `reuse: true`) a render is instead tied to a
fingerprint of what it shows, and stays a hit across file versions for as
long as that fingerprint is unchanged — editing one frame leaves the
cached renders of unrelated frames valid. Reuse is **off by default**:
until the rendering contract is confirmed against Figma's images endpoint
(below), a request without the opt-in behaves exactly as described
above and never reads or writes the fingerprint tables.

- **Fingerprint** (`images::render_fingerprint`, computed lazily at
  render time from the mirror, never during a pull): SHA-256 over the
  fingerprint-scheme version (`images::RENDER_FINGERPRINT_SCHEME`, 1),
  format, scale and three parts: (1) the node's subtree — every node's
  stored canonical raw JSON in preorder and child order, as nested
  `[raw, [children…]]` arrays; instance children arrive expanded in the
  REST response, variable and style values are baked into raw properties,
  image fills are referenced by content hash and fonts are named in text
  styles, so this covers nearly everything a single-node export shows.
  Node ids are part of the raw JSON, so a fingerprint belongs to exactly
  one node. (2) Conservatively, for every INSTANCE in the subtree whose
  main component is mirrored, that component's own subtree hash,
  transitively, so editing a main component invalidates its instances
  even where the expansion doesn't show the edit. (3) Conservatively,
  every ancestor's `explicitVariableModes`, root first.
- **Conservative fallback.** A node gets no fingerprint — and so only
  the per-version cache — when it, an ancestor or a subtree node is
  missing from the mirror, when an earlier sibling is a mask
  (`isMask`) that may clip it, or when the tree is inconsistent; also
  when the mirror has no version to pin the request to. With reuse on, a
  fingerprint miss still falls back to the per-version lookup.
- **Storage.** A fetched fingerprinted render is written as a
  `render_index` row (§3) plus an immutable, content-addressed
  `render_blobs` row keyed by the bytes' SHA-256 (the in-house
  implementation `figmog assets` uses — no new dependency), in one
  transaction, instead of an `images` row. The request is still pinned to
  the mirrored version (NWS-2960), which is the version the fingerprint
  was computed at. A hit requires the row's scheme, subject, format and
  scale to match, and the blob's bytes to still hash to the digest the
  row names (validated on every read; anything else is a miss and is
  refetched, repairing the blob).
- **Force refresh.** `--refresh` (tool: `refresh: true`) ignores every
  cached render, per-version and fingerprinted, and fetches again; a
  refetched fingerprinted render replaces its index row, and the blob it
  displaced is dropped once nothing references it. Fills are unaffected.
- **Eviction**, after every opted-in request that wrote a fingerprinted
  render or found orphans, in two separate passes over the index ordered
  oldest first (`seq`, then fingerprint as a deterministic tie-break;
  `images::plan_render_eviction`): **orphan cleanup** removes every row
  whose fingerprint no node has any more (its subject's current
  fingerprint differs, the subject is gone, or the row's scheme is
  stale); then a **hard byte quota** (`images::RENDER_CACHE_MAX_BYTES`,
  256 MiB of distinct blobs) removes the oldest remaining rows, still
  valid or not, until the cache fits. A blob goes when no remaining row
  references it. The version-change pass (§5) never touches these
  tables; `pull --fresh` wipes them with the rest of the store.
- **Unconfirmed contract.** The claim that a single-node export shows
  nothing outside the node's subtree (no ancestor effects) has not been
  checked against the live REST images endpoint — figmog's suite has no
  network access — which is why reuse stays opt-in. The fixtures in
  `images::tests` (text, fonts, instances and main components, image
  fills, inherited variable modes, sibling masks) pin what the
  fingerprint covers, not what Figma renders.

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

## 16. Project config and mirror roots

Source: `src/project.rs` (discovery, config, active-store rules),
`src/cli/migrate.rs` (`figmog migrate`), `src/cli/init.rs` (`figmog init`,
`figmog settings`), `src/cli/doctor.rs` (`figmog doctor`).

**Project root.** The nearest directory containing a regular file named
`figmog.toml`, walking up from the working directory but never past the
Git top level (a stray `figmog.toml` above a repository is ignored); else
the Git top level; else the working directory. Its `.figmog/` is the
*local root*: per checkout, it holds `current`.

**Mirror root.** Inside Git: `<git-common-dir>/figmog/`, from one
`git rev-parse --path-format=absolute --show-toplevel --git-common-dir`
(run via `std::process::Command` in the working directory; the path is
then canonicalized). The common dir is the same for the main worktree, its
subdirectories and every linked worktree, so all of them share one mirror
per file key; Git never tracks it, so it needs no ignore entry. Git older
than 2.31 echoes the unknown `--path-format=absolute` back and prints the
common dir relative to the working directory: the echo is dropped and the
relative path resolved against that directory. If Git is missing or the
directory isn't in a work tree (including bare repositories), figmog is
outside Git and the mirror root is the local root, `<project>/.figmog/`.

**Config.** `figmog.toml` and an optional `figmog.local.toml` beside it
(machine-specific; `pull` adds it to `.gitignore` alongside `.figmog/` when
the project has a `figmog.toml`). Both use the same hand-parsed TOML subset
(no `toml` dependency):

```toml
[files]
default = "AbCdEf1234"                                  # file key or Figma URL
library = "https://www.figma.com/design/XYZ/..."        # named entry

[verify]
dev_server = "http://localhost:3000"
```

Supported: `#` comments, blank lines, `[table]` headers, inside
`[files]` only `key = "string"` (bare or quoted keys; basic strings with
`\" \\ \n \t \r \uXXXX` escapes, or literal `'...'` strings), and inside
the settings tables `[stack]`, `[inspector]`, `[verify]`, `[tokens]` and
`[assets]` (below) `key = <value>` where the value is such a string, `true`/`false`,
or a decimal integer or float (`_` separators allowed), and inside
`[tokens.modes]` only `"<collection>/<mode>" = "<selector>"` (quoted keys,
string values; NWS-2948). Every `[files]` value
must be a file key or Figma URL and is stored as its key. Other tables,
top-level keys and `[[array]]` tables are skipped unparsed (reserved for
later settings, never an error), except that multi-line strings (`"""`
and `'''`) in skipped values are tracked so a header line inside one is
content, not a table. In the settings tables, a key this binary does not
know has its value skipped unparsed whatever its shape (arrays, inline
tables, dates, multi-line strings — tracked across lines by bracket depth
outside strings, so a header line inside one is content); a *known* key's
value must be a single-line string, boolean or number, and multi-line
strings, arrays and inline tables there are an error. Errors — unterminated headers, strings or
multi-line strings, a missing `=`, a value of the wrong type, trailing
text, an invalid file reference or setting, a duplicate key or a second
copy of a known table, an unknown key's value left open at the end of
the file (`unterminated value`) — read `figmog.toml:<line>: <message>` (or
`figmog.local.toml:...`); a settings error names its key
(`figmog.toml:7: verify.threshold: must be between 0 and 100, found
500`). Local entries replace same-named ones, key by key in the settings
tables too. In `[files]`, `default` is the *product* file: the file every
command uses when none is given, ahead of `.figmog/current`, which remains
the fallback (and is still written by `pull` and serve's default session).
`library` names the optional library file holding tokens and components
(for later features). Other named entries are parsed and validated but
not yet addressable.

**Settings (NWS-2946).** The typed settings later features read
(`project::Settings`, through `ProjectConfig::settings` /
`Layout::settings`; `figmog settings` for tools outside the binary).
Every key is optional; a missing key is its default:

| key | type | default | rule |
|---|---|---|---|
| `stack.framework` | string | `"react"` | v1: `react` only |
| `stack.styling` | string | `"tailwind"` | v1: `tailwind` only |
| `stack.tokens` | string | `"src/figmog-tokens.css"` | relative path inside the project: no `..`, not absolute, no `\`, last segment a file name (not `.`, `..` or empty) |
| `inspector.enabled` | boolean | `true` | the dev-server inspector, development builds only |
| `inspector.shortcut` | string | `"Alt+Shift+F"` | one or more of `Ctrl`/`Alt`/`Shift`/`Meta` (any case, each once), `+`-joined, then one ASCII letter or digit, or `F1`–`F12` |
| `inspector.attributes` | boolean | `false` | the `@figmog/react` Vite plugin adds `data-figmog`/`data-figmog-node` to mapped components' root elements, development builds only (§20); off by default: explicit locators come first |
| `verify.loop` | boolean | `true` | the verify loop on or off |
| `verify.dev_server` | string | `"http://localhost:5173"` | `http`/`https`, no credentials, loopback host only (`localhost`, `*.localhost`, `127.0.0.0/8`, `[::1]`): v1 is local only |
| `verify.threshold` | number | `95` | advisory match score, 0–100 (the screenshot comparison's `score`) |
| `tokens.format` | string | `"tailwind"` | `tailwind` (CSS variables plus a Tailwind v4 `@theme`) or `css` (CSS variables only); DTCG and TypeScript are not v1 (NWS-2948) |
| `assets.icons` | string | `"src/icons"` | the icon components' folder (`svg/` beneath it holds the SVGs); a relative path inside the project, like `stack.tokens` (NWS-2952) |
| `assets.images` | string | `"public/figma"` | the image fills' folder; same rule |
| `assets.manifest` | string | `"figmog.assets.json"` | the generated asset manifest `figmog spec` reads; same rule |
| `assets.prefix` | string | `"Icon"` | every icon component name starts with it: an ASCII capital letter, then up to 31 ASCII letters or digits |
| `assets.current_color` | boolean | `false` | convert single-color icons to `currentColor` (opt-in; multicolor icons always keep their colors) |
| `assets.components` | string | `"tsx"` | `tsx` or `jsx` |

`[tokens.modes]` (NWS-2948) maps variable-collection modes to the
selectors `figmog tokens` writes them under, one `"<collection>/<mode>" =
"<selector>"` per mode: `":root"` makes it the collection's root mode,
`""` leaves it out, anything else must be one CSS selector (no surrounding
spaces, `,`, `{`, `}`, `;`, `@` or comments). Keys must contain a `/`;
whether they name a real mode is checked against the snapshot at export
time (§9 "Token export, diff and check"). Modes not named keep the default mapping.
`figmog.local.toml` replaces single mappings. `figmog init` writes neither
`[tokens]` table, nor `[assets]` (§19).

```toml
[tokens]
format = "tailwind"

[tokens.modes]
"Theme/Dark" = '[data-theme="dark"]'   # the default for a dark-like mode
"Brand/Globex" = ".brand-globex"
"Density/Spacious" = ""                  # not exported
```

The verify contract is fixed for v1 and not configurable: viewports 1440
and 390 CSS px wide, `light` color scheme, `default` state
(`VERIFY_VIEWPORTS`, `VERIFY_COLOR_SCHEME`, `VERIFY_STATE`). A key this
binary does not know inside a settings table is kept, not rejected
(`ProjectConfig::unknown_keys`, sorted `<table>.<key>`), so a newer
figmog's settings never break an older binary; its value, of any shape,
is skipped (above). `figmog doctor` reports unknown keys as a warning.

`figmog settings` prints `{project_root, config (the figmog.toml path or
null), files, unknown_keys, stack: {framework, styling, tokens,
tokens_path}, inspector: {enabled, shortcut, attributes}, verify: {loop, dev_server,
threshold, viewports, color_scheme, state}, tokens: {format, modes}}`
(`tokens_path` is absolute).
It does not take `--db`.

**Active store.** The mirror root is where mirrors *should* live; the
active store is where a key's mirror *is*. Outside Git both are the same
directory. Inside Git, for each key:

| `.figmog/<key>/db` (old) | `<mirror root>/<key>/db` (shared) | result |
|---|---|---|
| exists | absent | use the old store; stderr `{"warning": "migration pending: ..."}` |
| absent | exists | use the shared store |
| absent | absent | new mirror in the shared store |
| exists | exists | error naming both paths (never a silent pick) |

With both present and a `migrated-from` marker in the shared `<key>/`,
the error instead says a `figmog migrate` was interrupted and to rerun it.
The warning is printed by every direct CLI command that resolves the old
store and by serve when a session opens it; JSON outputs are unchanged.
The old store is resolved before its lock is taken, so opening it never
creates it: once its lock is held it must still exist and still be the
active store, otherwise (a `figmog migrate` moved it meanwhile) the
command fails asking for a rerun, which resolves the shared store. A
missing old store is never recreated; new mirrors always go to the shared
root.
`--db` bypasses all of this. `figmog serve --figmog-root <dir>` (hidden,
for tests) pins the socket and every store to `<dir>`/`<dir>/<key>/db`.

**Socket location.** One background owner per mirror root serves every
worktree (§6b); its lock, discovery record and log live in the mirror root
and its socket under `/tmp/figmog-<uid>/`. An in-process serve
(`--figmog-root`) still binds `<root>/serve.sock`.

**`figmog migrate [FILE]`.** Moves each old `.figmog/<key>/` directory
(all of it: today that is `db/`, whose image cache is an in-store sink) to
`<mirror root>/<key>/`; with no `FILE`, every old mirror plus every
interrupted move, in sorted key order. Prints
`{"mirror_root", "migrated": [{"file", "from", "to", "resumed"}]}`.
Outside Git it is a no-op. Per key:

0. Take the repository-wide migration lock
   `<mirror root>/<key>.migrate.lock` (every worktree shares the
   destination and staging dir), held until the end; while another
   migration of the key holds it — from any worktree — this one is
   refused with a message to rerun once it finishes.
1. Refuse if `<mirror root>/<key>` already exists.
2. Open the old store through `open_store_checked` — its ownership anchor
   *and* fjall's own writer lock, so a running serve (even an older binary)
   fails this with the standard "store is locked" error — then close the
   database but keep the anchor.
3. Remove any staging dir `<mirror root>/.<key>.migrating` left by an
   interrupted run (safe: the migration lock excludes a live one), copy the old directory into it (regular files and
   directories only, fsynced; the lock anchor is not copied), take a fresh
   anchor on the staged `db/`, and write `migrated-from` naming the source.
4. Atomically rename the staging dir to `<mirror root>/<key>`; the staged
   anchor (still held) moves with it, so the destination is owned the
   moment it appears.
5. Atomically rename the old directory to its tombstone
   `.figmog/.<key>.deleting` beside it (its anchor is still held and its
   fjall lock was checked in 2), remove the tombstone recursively, then the
   marker. A tombstone is never opened as a store.

A crash during 3 leaves the old store intact and active (the staging dir
is ignored and redone on rerun). A crash after 4 leaves the marker, which
the active-store rules report and a rerun finishes by doing 5 while holding
both stores' anchors *and* fjall writer locks (as in 2, so an older
binary's serve on either store refuses it). The marker names the source
checkout's old directory, and only a rerun from that checkout removes it:
another worktree's own old mirror is an independent copy, so there the
rerun refuses, naming the checkout that must finish — unless the recorded
source is already gone, when it just removes the source's tombstone (if
any) and the marker. A crash during 5's deletion leaves a half-deleted
tombstone, which a rerun removes without opening it. An old directory an
older binary's interrupted step 5 left half deleted in place is still
checked for anchors and fjall locks, but failing to open as a store (it is
not lock contention) does not stop the rerun from retiring it. A stray
tombstone with no marker is removed by any rerun.

**`figmog init` (NWS-2946).** One non-interactive command from a React
+ Tailwind repository to a working mirror and MCP registration
(`src/cli/init.rs`). It never prompts; every answer is a flag or
detected: `--file <key|URL>` (the product file; default: `figmog.toml`'s
`[files] default`, else `.figmog/current` — never `figmog.local.toml`'s,
which is machine-local: with no `[files]` in `figmog.toml`, no
`.figmog/current` and no `--file`, `init` asks for `--file`), `--library`, `--framework`,
`--styling`, `--tokens`, `--dev-server`, `--token-stdin`, `--from-file
<saved response>` (offline first pull), `--no-pull`, `--mcp-command`.
It does not take `--db`. Steps, in order, each idempotent:

1. Validate every flag (reading and shape-checking a `--token-stdin`
   token) before writing anything, then discover the project (above):
   `init` writes at the existing `figmog.toml`'s directory; with none up
   the tree, at the working directory when it has its own `package.json`
   (a monorepo package such as `apps/web` — consistent with discovery,
   which walks up from there and finds the new `figmog.toml` first; the
   mirror root stays the repository's), else the Git top level, else the
   working directory. Commands run outside that package (say, from the
   repository root) do not see its `figmog.toml`, and old mirrors in
   another directory's `.figmog/` are not migrated.
2. `figmog.toml`: created whole (a header comment, then `[files]`,
   `[stack]`, `[inspector]`, `[verify]` with every key written), or, when
   it exists, only the known tables it lacks are appended after a blank
   line — an existing table is never edited, and a flag that contradicts
   an existing table's value (its default when the key is absent) is an
   error naming the key. An existing `[files]` without `default` is an
   error. The stack is detected only for a new `[stack]`: React from a
   `react` key in `package.json`'s `dependencies`, `devDependencies` or
   `peerDependencies` (searched in that order); Tailwind from a
   `tailwindcss`, `@tailwindcss/vite` or `@tailwindcss/postcss`
   dependency, else a `tailwind.config.{js,cjs,mjs,ts,cts,mts}` file;
   `--framework`/`--styling` skip detection. Not finding both is an error
   (v1 supports React + Tailwind only) naming the flags to pass. The file
   is read back through discovery before continuing.
3. Token: `--token-stdin` saves it through `auth::login` (§10
   "Credentials"); otherwise `auth status` is reported.
4. `.gitignore`: `.figmog/` and `figmog.local.toml`, by the §10 "Git
   ignore rules".
5. `figmog migrate` (every key) moves old `.figmog/<key>/db` mirrors to
   the shared root (above).
6. `.mcp.json` at the project root (Claude Code's project scope): adds
   `mcpServers.figmog = {"type": "stdio", "command": "figmog" (or
   --mcp-command), "args": ["serve"], "env": {}}`. An identical entry is
   left alone; a different one is kept unless `--mcp-command` was given
   (then replaced); every other key is preserved. The file is rewritten
   (sorted keys, two-space indent, trailing newline) only when the entry
   changes. Invalid JSON or a non-object is an error, never overwritten.
   v1 registers Claude Code only and never runs `claude`.
7. `AGENTS.md`: figmog's block — how to read the design (MCP tools, CLI),
   the stack and token file from the settings, the v1 verify contract,
   what never to commit — between `<!-- figmog:begin (managed by `figmog
   init`; edits inside are replaced) -->` and `<!-- figmog:end -->`.
   Created when absent, appended (after a blank line) when it has no
   markers, replaced in place when it has exactly one begin marker before
   one end marker; any other marker layout is an error. Nothing outside
   the markers changes.
8. The first pull of the project's default file as every later command
   resolves it (`figmog.local.toml`'s `[files] default` when it overrides
   the committed one), exactly as `figmog pull <key> [--from-file]`
   (socket-routed when a serve answers, else direct; with the global
   `--no-socket`, no socket is looked for or contacted) —
   last, so a failing pull (say, no token) leaves the rest set up, which
   its error says. `--no-pull` skips it.

Output: `{project_root, config: {path, status: created|updated|unchanged,
added_tables}, files: {default, library}, stack: {framework, styling,
tokens, source: {framework, styling}}, token, gitignore: {updated},
migrated, mcp: {path, status: added|updated|kept|unchanged, server},
agents: {path, status: created|appended|updated|unchanged}, pull:
{skipped, file?, churn?}}`. `files` and `stack` — and the `AGENTS.md`
block — come from `figmog.toml` alone: machine-local overrides in
`figmog.local.toml` are never committed. A rerun with the same flags
rewrites no file.

**`figmog doctor` (NWS-2946).** Checks the setup (`src/cli/doctor.rs`) and
prints `{status, checks: [{name, status, detail}]}`, the checks in this
fixed order, each `ok`, `warn` or `fail`; `status` is the worst. It
discovers the project itself, so a malformed config is a failed check
rather than an exit before the report. `--upstream <url>` names the
desktop MCP server (default as for `serve`); `--db` is refused.

| check | ok | warn | fail |
|---|---|---|---|
| `config` | `figmog.toml` read | no `figmog.toml` (defaults apply); unknown settings keys | malformed config |
| `token` | a token resolves | — | none, or the credential store fails |
| `mirror` | the product file's active store exists | no product file; not pulled yet; migration pending | invalid `.figmog/current`; two mirrors |
| `file_access` | `GET /v1/files/:key/meta` answers | network error or rate limit; skipped (no token or no product file) | 403 or another HTTP error |
| `variables_access` | `GET /v1/files/:key/variables/local` answers | 403/404 (not on this plan: use `import-variables`); network or other error; skipped | — |
| `desktop_mcp` | the desktop server initializes | unreachable (figmog's own tools still work) | — |
| `mcp_config` | `.mcp.json` has a `figmog` server whose command is an executable path or on `PATH` | no file, no entry, or an unresolvable command | unreadable or invalid JSON |
| `dev_server` | a TCP connection to `[verify] dev_server`'s host and port succeeds, or the verify loop is off | not reachable; name resolution failed or timed out | — |

The store is never opened (a running serve may hold it). The dev
server's `localhost` / `*.localhost` host goes straight to `127.0.0.1` and
`::1` without a resolver, an IP literal as is; any other name is resolved
on a detached thread abandoned after 2 s (a `warn`). Network checks
are bounded — 10 s per Figma request, the desktop client's 2 s connect
and 10 s request timeouts, 2 s per dev-server address — and a network
failure is a `warn` ("could not reach Figma"), so an offline machine gets
a complete report quickly. Figma checks use serve's API client (debug
builds honor `FIGMOG_API_BASE`, for tests). The token is never printed.
Exit 0 unless a check failed; then the report is still printed and
`{"error": "doctor: failed checks: <names>"}` goes to stderr, exit 1.

## 17. Code-ready spec (NWS-2968, NWS-2947)

`figmog spec <node> [--tailwind | --no-tailwind]` and the
`figmog_spec {id, tokens?, tailwind?, map?}` tool describe one subtree — a
card, a page section — the way a developer builds it. Source:
`src/spec.rs` (pure) and `src/spec/css.rs` (the CSS declarations and the
Tailwind hint), composed in `query::spec`. NWS-2968 was the slice (one
auto-layout card); NWS-2947 grew it to the full description below.

**How it reads.** A local read like any other (`ReadCommand::Spec`, §6
"Read adapters"): direct CLI, socket/owner-routed CLI, `figmog call` and
MCP all return the same JSON, and the NWS-2902 parity tests cover it. It
runs on a read worker. The inputs are exactly the `figmog_subtree {id}`
answer (raw JSON, children nested), the `figmog_vars {snapshot: true}`
answer, and the `components`/`component_sets` records of the subtree's
instances, so the output is a function of the mirror alone. `id` takes a
bare id, a `12-34` id or a Figma URL (§12); an unknown id is the standard
`no node … in the mirror` error.

**Project settings.** Token CSS names and root modes come from the token
export (§9) run with the project's `[tokens]` settings (NWS-2948), so
they match the committed token file; `figmog_spec` takes them as an
optional `tokens: {format?, modes?}` argument (validated like
figmog.toml's `[tokens]`/`[tokens.modes]`). The Tailwind class hint
(`tailwind: true`) is on when the project's `[stack] styling` is
`tailwind` (§16; v1's only styling, so on in every project), and
`--tailwind`/`--no-tailwind` (or an explicit `tailwind` argument)
override it. The Figma → code component map (§18) is the optional `map`
argument: the content of `figmog.map.json`, validated exactly like the
file. The asset manifest (§19) is the optional `assets` argument: the
manifest's content (or its normalized `{version, icons: [{node_id, key,
component, path, svg}], images: [{image_ref, path, url}]}` form),
validated strictly (node ids, imageRefs, component names, project-relative
paths, root-relative URLs). `figmog spec` in a project and the stdio relay
(`figmog serve`'s default shape) fill in whichever of the four the caller
left out (`map` from the project root's `figmog.map.json` and `assets`
from `[assets] manifest`, each read on every call; when there is none,
none; when it is invalid, the call fails naming the file and JSON path);
with `--db`, `figmog call`, or an MCP client of an in-process serve, an
omitted `tokens` is the default mapping, an omitted `tailwind` is off and
an omitted `map` or `assets` is none.

**Output.** One JSON object, every key sorted (`serde_json` without
`preserve_order`); byte-identical across runs for the same mirror:

```
{
  "node": <spec node>,
  "tokens_snapshot": {state, source, format, format_version, exported_at,
                      digest, variables, collections, theme, css_unavailable},
  "unsupported": [{"id", "reason"}, ...]   // sorted by id, then reason
}
```

`tokens_snapshot` is the §9 snapshot summary the names refer to, plus the
theme the token export picks (`{collection, light_mode, dark_mode}` or
null) and `css_unavailable`: null, or the token export's error (no
`current` snapshot, a name collision, an ambiguous theme), in which case
every `css` name is null and bound values are written raw in the CSS.

### Two layers

A spec node is

```
{id, name, type,
 design?,                      // what Figma recorded
 layout?, values, content?, instance?, css?, tailwind?,   // figmog's reading
 children}
```

with `children` in child-index order. A hidden layer (`visible: false`)
keeps only `{id, name, type, design?}`.

- **`design`** is the recorded layout, copied verbatim (never
  normalized): `layoutMode`, `layoutWrap`, `layoutPositioning`,
  `layoutAlign`, `layoutGrow`, `layoutSizingHorizontal`/`Vertical`,
  `primaryAxisSizingMode`, `counterAxisSizingMode`,
  `primaryAxisAlignItems`, `counterAxisAlignItems`,
  `counterAxisAlignContent`, `itemSpacing`, `counterAxisSpacing`,
  `itemReverseZIndex`, `strokesIncludedInLayout`, `padding*`,
  `min/maxWidth`, `min/maxHeight`, `constraints`, `clipsContent`,
  `rotation`, `absoluteBoundingBox`, `textAutoResize`, the twelve `grid*`
  fields, and for TEXT `style: {textAutoResize, textTruncation, maxLines,
  textAlignHorizontal, textAlignVertical}` — whichever the node has.
- **figmog's interpretation** is everything else: `layout` and `values`
  (value entries, below), `content`, `instance`, and `css` — a
  `{property: value}` map of the declarations figmog derives, a bound
  value as its token's `var(--name)`. `tailwind` is a class string derived
  from exactly those declarations.

What figmog can't translate exactly is listed under `unsupported` and
left out of `css` — never approximated. The design layer still records
it, and the node's other values and children are still specified.

### Layout

`layout` (on auto-layout and grid frames, their children, absolutely
positioned nodes, and the root when it carries `layoutSizing*`):

- `direction`: `horizontal` | `vertical`; `wrap: true` and `wrap_gap`
  (the `counterAxisSpacing` entry; Figma's default, the item spacing, when
  absent) for `layoutWrap: WRAP`;
- `gap`: a value entry for `itemSpacing`, or `"auto"` under
  `SPACE_BETWEEN` (a bound `itemSpacing` there is still shown, as
  `item_spacing`, and listed: CSS doesn't use it);
- `grid` (`layoutMode: GRID`): `{columns, rows, column_gap, row_gap}` —
  the counts and the `gridColumnGap`/`gridRowGap` entries;
- `padding`: `{top, right, bottom, left}` value entries (an absent padding
  is Figma's 0);
- `align`: `{primary, counter}` from `primaryAxisAlignItems` /
  `counterAxisAlignItems` — `start`, `center`, `end`, `space-between`,
  `baseline` (absent ⇒ `start`);
- `sizing`: `{horizontal, vertical}`, each `hug` | `fill` | `fixed` —
  `layoutSizingHorizontal`/`Vertical` when present, else the older fields:
  `layoutGrow > 0` along the parent's direction or `layoutAlign: STRETCH`
  across it ⇒ fill; the node's own `primaryAxisSizingMode` /
  `counterAxisSizingMode` (absent ⇒ `AUTO`) ⇒ hug or fixed; a TEXT node's
  `textAutoResize` (`WIDTH_AND_HEIGHT` both axes, `HEIGHT` vertical) ⇒
  hug; else fixed;
- `width`/`height`: value entries for a fixed dimension the CSS declares
  (bindings: `boundVariables.size.x`/`.y`, else `width`/`height`); a bound
  dimension the CSS doesn't declare (a fill/hug sizing, a LEFT_RIGHT or
  SCALE constraint) is still shown as its token here, and listed;
- `min_width`, `max_width`, `min_height`, `max_height`: value entries;
- `position: "absolute"` for an absolutely positioned node;
- `size`: `{width, height}` from `absoluteBoundingBox`, whenever `layout`
  is present.

### Values

`fill` and `stroke` (arrays, one entry per visible `SOLID` paint; the raw
value is `#rrggbb`, or `#rrggbbaa` below full alpha, the paint's `opacity`
folded into alpha — the token export's color form), `stroke_weight` (with
visible strokes), `radius` (`{top_left, top_right, bottom_right,
bottom_left}` from `rectangleCornerRadii`, else `cornerRadius`; absent
when the node has neither and no corner binding), `opacity` (a layer
opacity below 1, or bound; matched against tokens as 0–100, Figma's
opacity-variable scale), `effects` (one object per visible effect: `type`
plus `offset_x`, `offset_y`, `radius`, `spread`, `color` entries for
shadows, `radius` for blurs), and on TEXT nodes `typography`:
`font_family`, `font_size`, `font_weight`, `line_height` (`lineHeightPx`;
`"auto"` for `INTRINSIC_%`), `letter_spacing`, each present when the type
style has it.

**Value entries.** Bound:
`{"token": <variable name | null>, "variable_id", "css": <name | null>,
"raw": <Figma's baked value>, "bound_at": [<JSON pointer>, ...]}` —
`bound_at` lists (sorted) the RFC 6901 pointers, relative to the node's
raw JSON, of the bindings read for it (`/fills/0/boundVariables/color`,
`/boundVariables/fills/0`, `/styleOverrideTable/1/boundVariables/fontWeight`…). Bindings are read from the paint's
`boundVariables.color`, else the node's `boundVariables.fills[i]` /
`strokes[i]`; `itemSpacing`, `counterAxisSpacing`, `gridColumnGap`,
`gridRowGap`, `paddingTop`…, `strokeWeight`, `topLeftRadius`… (else
`rectangleCornerRadii.RECTANGLE_*`), `size.x`/`size.y` (else `width`/
`height`), `minWidth`…, `opacity`; an effect's own `boundVariables`
(`offsetX`, `offsetY`, `radius`, `spread`, `color`); text properties from
the node's `boundVariables`, else its type style's (`fontFamily`,
`fontSize`, `fontWeight`, `lineHeight`, `letterSpacing`; a mixed-run
node and each run read the style's first), an alias or an array of
aliases naming one variable. `token` is the variable's name in the
snapshot (null for a variable bound in the file but absent from the
snapshot); `css` is the custom property name `figmog tokens export` emits
for it (§9, taken from that export run on the same snapshot, so they
cannot drift) — null for a variable the export skips (STRING, BOOLEAN…).
An alias variable is shown as itself (`surface`, not the primitive it
aliases).

**Token claims only from bindings.** Unbound: `{"untokenized": true,
"raw", "matches"?}` — never token usage, and the CSS uses the raw value
(`8px`, never `var(--spacing-2)`). `matches` lists (sorted by token name,
then id) every snapshot variable `{token, variable_id, css}` whose value
in its root mode — the mode the token export writes to `:root` under the
project's mapping (§9), else its collection's default mode, aliases
followed — equals the raw value (colors as `#rrggbb[aa]`, numbers within
1e-6, strings exactly), of the property's type, and scoped for it:
`ALL_SCOPES`, or `ALL_FILLS` plus `TEXT_FILL` / `FRAME_FILL` /
`SHAPE_FILL` by node type for fills, `STROKE_COLOR`, `STROKE_FLOAT`,
`CORNER_RADIUS`, `GAP` (gaps and padding), `WIDTH_HEIGHT` (sizes and
min/max), `OPACITY`, `EFFECT_COLOR`, `EFFECT_FLOAT`, `FONT_FAMILY`,
`FONT_SIZE`, `FONT_WEIGHT`, `LINE_HEIGHT`, `LETTER_SPACING`. Omitted when
nothing matches.

**No binding disappears silently.** Every `VARIABLE_ALIAS` in a node's
raw JSON (children excluded) is either read — and then shown in a bound
entry's `bound_at` — or listed under `unsupported` by its pointer
(`/boundVariables/visible: bound to a variable: not specified`; a binding
a hidden paint carries, a gradient stop's, `textRangeFills`…). A
property bound in two places to different variables is listed too. A
mixed-run node's node-level per-range arrays count as read when the base
style and the runs' overrides name every variable in them.

### Content and instances

- `content.text`: a TEXT node's `characters` (`content.text_token`, a
  bound entry, when `characters` is bound).
- `content.runs` (mixed text runs: `characterStyleOverrides` naming a
  style other than 0): every run in order, `{start, end, text, style}`
  (offsets in UTF-16 code units, as Figma's; `style` the override id, 0
  the base style), plus for an override its `typography` and `fill`
  entries (only the properties the override sets), `css` and `tailwind`.
- `content.assets`: `{kind: "image", node_id, paint, image_ref,
  scale_mode}` per visible `IMAGE` fill; `{kind: "vector", node_id,
  format: "svg"}` for a node CSS can't draw (`VECTOR`,
  `BOOLEAN_OPERATION`, `STAR`, `LINE`, `REGULAR_POLYGON` — one asset, its
  children are part of it and not specified; its fills/strokes are still
  value entries but get no CSS); `{kind: "export", node_id, formats}` for
  a node with `exportSettings` (`PNG@2x`…). Without an asset manifest
  (§19), assets are referenced by node id and `imageRef` only. With one,
  an exported image fill's entry also carries the file's `path` and
  `url` (null outside `public/`), and its `unsupported` reason says it
  was exported (its scaling and cropping still have no CSS); a node that
  is an exported icon — for an instance by its main component's key
  (keys are the same in every file using a library component), else by
  node id, but only when the manifest's icon was exported from the file
  this spec describes (node ids are file-local and repeat across files;
  the CLI and the relay tell the manifest which file the call targets,
  as its `for_file`) — gets `{kind: "icon", node_id,
  component, path, svg, jsx: "<IconX />"}` (instead of the `vector`
  entry, for a vector node). Without a manifest the output is unchanged.
- `instance` (INSTANCE nodes): `{component_id, component: {name, key,
  remote} | null, component_set: {id, name, key} | null, variant: {name:
  value} (VARIANT properties), properties: {name: {type, value, token?}}
  (the other component properties, `token` a bound entry), code_component}`.
  `code_component` is the code component the map (§18) gives the
  instance's component or its set — `{figma, path, export, props, jsx,
  coverage, unmapped_properties, unmapped_overrides, unknown_props}` — or
  null when there is no map or no entry for it. A component missing from
  the mirror is listed under `unsupported`. The instance's children are
  still specified in full either way.

### The CSS layer

Per node, from the parent's layout and its own:

| design | `css` |
|---|---|
| auto layout | `display: flex`, `flex-direction`, `gap` (`column-gap` + `row-gap` when wrapping), `justify-content` (non-start), `align-items` (always: CSS's default is stretch), `flex-wrap` + `align-content` |
| negative `itemSpacing` (no wrap) | no gap; `margin-left`/`margin-top` = the spacing on every in-flow child after the first (exact: the children overlap the same way) |
| `layoutMode: GRID` | `display: grid`, `grid-template-columns`/`-rows` = `gridColumnsSizing`/`gridRowsSizing` (Figma's own CSS track lists), `column-gap`, `row-gap`; a child gets `grid-column`/`grid-row: <anchor + 1> / span <span>` and `justify-self`/`align-self` from `gridChild*Align` |
| padding | `padding` when all four sides are equal, else the longhands; an unbound 0 is left out |
| sizing in a flex parent | main axis: fill → `flex: 1 1 0%` + `min-width: 0` (`min-height: 0` in a column: Figma lets a fill child shrink to nothing), fixed → `width`/`height` + `flex-shrink: 0`, hug → `flex-shrink: 0`; cross axis: fill → `align-self: stretch`, fixed → the dimension |
| sizing in a grid parent | fill → `justify-self`/`align-self: stretch`, fixed → the dimension, hug → `fit-content` |
| sizing of the root | fixed → the dimension, horizontal hug → `width: fit-content`, fill → `100%` |
| `min/max*` | `min-width`… |
| absolute child (`layoutPositioning: ABSOLUTE`, or any child of a frame without auto layout) | `position: absolute` (the parent gets `position: relative`); per axis from `constraints` (default LEFT/TOP) and the boxes: LEFT/TOP → `left`/`top`, RIGHT/BOTTOM → `right`/`bottom`, LEFT_RIGHT/TOP_BOTTOM → both, CENTER → `calc(50% ± offset)` + `translate(-50%)`, SCALE → percentages for offset and size; a fixed dimension otherwise. Inside a parent with a CSS border (a stroke included in layout) the offsets are reduced by the border width (CSS measures them from the padding box); SCALE there is unsupported |
| `clipsContent` (with children) | `overflow: hidden` |
| fill | one visible solid paint → `background-color` (`color` on TEXT) |
| stroke | one visible solid paint with `strokeAlign`: `outline-*` (Figma strokes take no layout space), `outline-offset` −weight (INSIDE) or −weight/2 (CENTER); a `border-*` for INSIDE with `strokesIncludedInLayout` |
| radius | `border-radius`, or the corner longhands |
| one drop/inner shadow | `box-shadow` (`inset`); a TEXT drop shadow → `text-shadow`. Two or more shadows are unsupported: CSS lists them top-most first and Figma's effect order is unverified |
| opacity | `opacity` |
| typography | `font-family` (quoted), `font-size`, `font-weight`, `line-height` (`normal` for auto), `letter-spacing`, `font-style: italic`, `text-align`, `text-transform` / `font-variant-caps`, `text-decoration-line` |
| text | `white-space: pre-wrap` (Figma keeps newlines and runs of spaces); `textAutoResize: WIDTH_AND_HEIGHT` → `pre`; `HEIGHT` and `NONE` follow from the sizing |
| `textTruncation: ENDING` | one line: `overflow: hidden; text-overflow: ellipsis; white-space: pre`; `maxLines` > 1: `-webkit-line-clamp` |
| padding or border | `box-sizing: border-box` (Figma sizes include both) |

Lengths are `px` with at most three decimals; a bound value with a CSS
name is its `var()` — matched to the unit the token export writes it in
(§9): a unitless FLOAT (e.g. `ALL_SCOPES`) used as a length is
`calc(var(--x) * 1px)`, a unitless one used as opacity (Figma's 0–100)
`calc(var(--x) / 100)`, a `%` one as opacity as is; a unit the property
can't take (a `%` token as a gap, a px token as a font weight) is listed
and left out.

**Tailwind hint** (`tailwind`). One class per declaration, ordered by
property group (position, layout, size, paint, text): keyword utilities
(`flex`, `flex-col`, `items-center`, `self-stretch`, `shrink-0`,
`flex-1`, `absolute`, `overflow-hidden`, `line-clamp-2`, …); a value bound
to a token with a Tailwind theme name in the right namespace becomes that
theme utility (`--color-surface` → `bg-surface`, `--spacing-4` →
`gap-4`, `p-4`, `--radius-md` → `rounded-md`); any other bound value the
arbitrary property `[font-size:var(--font-size-title)]`; an unbound value
an arbitrary value (`gap-[8px]`, `bg-[#1a1a1f]`) — never a theme utility.
`pt-X pb-X` / `pl-X pr-X` collapse to `py-X` / `px-X`. `box-sizing:
border-box` has no class (Tailwind's preflight sets it).

### Unsupported, never approximated

Each case is listed under `unsupported` with the node id and reason, and
the affected part is left out of `css` (and, where noted, of the spec):

| case | left out |
|---|---|
| `visible: false` | the node's layout, values and children |
| `layoutMode` other than `HORIZONTAL`/`VERTICAL`/`GRID`/`NONE` | its own layout; children unplaced |
| a GROUP (or other non-frame) with children | its children's positions (they follow the enclosing frame's constraints) |
| `layoutWrap: WRAP` on a vertical frame | the wrapping |
| negative `itemSpacing` in a wrapping frame; negative `counterAxisSpacing` or grid gap | that gap |
| `gridColumnsSizing`/`gridRowsSizing` missing or not a plain track list | that template |
| `itemReverseZIndex` | the stacking order |
| an absolute child before in-flow siblings | the stacking order (CSS paints it above them) |
| `rotation` on an absolutely positioned node | its position and size |
| `rotation` on any other node | the rotation (its CSS size is the rotated bounds) |
| `layoutPositioning: ABSOLUTE` on the spec root | its position in its parent |
| a visible `IMAGE` fill | the CSS background (it is under `content.assets`) |
| any other non-`SOLID` paint (gradients, video…) | that paint |
| several visible fills or strokes | that CSS property |
| a non-normal `blendMode` (layer or paint) | the blend |
| a property bound to several variables (mixed alias array) | that property |
| a binding the spec doesn't read (by pointer), or a property bound to different variables in two places | that binding |
| a bound size or `itemSpacing` the CSS doesn't use (sizing, constraints, `SPACE_BETWEEN`) | the token in CSS (still shown as a token reference) |
| a token whose exported unit the property can't take | that declaration |
| two or more shadows | the `box-shadow` / `text-shadow` |
| a SCALE constraint inside a CSS border | that offset |
| a vector-like node's children | the children (part of the asset) |
| `LAYER_BLUR`/`BACKGROUND_BLUR` (no exact CSS `blur()`), `showShadowBehindNode`, a non-normal shadow blend, an inner or spread shadow on text | that effect |
| `individualStrokeWeights`, `strokeDashes`, a text stroke, a missing `strokeAlign` | the CSS stroke |
| `cornerSmoothing` | the smoothing |
| `textAlignVertical` other than TOP in a fixed-height text box, `paragraphSpacing`, `paragraphIndent`, list text (`lineTypes`) | that text rule |
| mixed runs splitting a UTF-16 surrogate pair, a run style missing from `styleOverrideTable` | the runs / that run's style |
| a component not in the mirror | the instance's component name |

### Fixtures and tests

`tests/fixtures/spec/card.json` (a vertical auto-layout card: a
horizontal row using the older sizing fields, a CSS-grid gallery with an
image fill, an absolute ribbon with a gradient, a mixed-run caption) and
`tests/fixtures/spec/edge.json` (negative spacing, wrap, a 2×2 grid with
a span and an aligned cell, min/max sizes, a frame without auto layout
with every constraint, a rotated child and clipping, text auto-resize,
truncation, line clamp and mixed runs, an instance with variant
properties and a vector icon, an image fill with a shadow and opacity, an
absolute ribbon, plus the review cases: unitless and `%` tokens used as
lengths and opacity, a bound size under a LEFT_RIGHT constraint, a
`SPACE_BETWEEN` row with a bound spacing, an absolute child inside a CSS
border, two shadows, a text with a newline), with
`tests/fixtures/spec/variables.json` (card) and `edge-variables.json`
(the same plus unitless/`%` FLOATs), produce the golden `card.spec.json`
(no hint) and `edge.spec.json` (with the Tailwind hint), and `tokens.css`
(the token export of the edge variables)
(`tests/spec.rs`; `FIGMOG_BLESS=1` regenerates them). The tests check
byte-identical output across runs and against `figmog call figmog_spec`,
the issue's done-when line on both fixtures — every binding in the
subtree, by pointer, appears in a token reference's `bound_at` or is
listed under `unsupported` by that pointer, every value entry without one is
flagged untokenized, each `css` name appears in the `tokens export`
output — and the project defaults for `tokens` and `tailwind`.

**The two contract viewports.** The spec is viewport-independent JSON;
what can differ by viewport is its translation. `packages/react`'s
`test/spec.test.ts` renders `edge.spec.json` in Chromium twice — each
node's `css` inline, and its Tailwind hint compiled by Tailwind v4 against
`tokens.css` — at 1440 px and 390 px, and asserts that every node's box
(relative to the root) equals its recorded `absoluteBoundingBox` within
half a pixel, except nodes under a position-related `unsupported` entry
and the width of a hugging text node (font-dependent); and that bound
tokens (including unitless and `%` ones scaled for padding and opacity),
clipping, truncation, run styles and kept newlines resolve.

## 18. Figma → code component map (NWS-2949)

Source: `src/map.rs` (pure: validation, the per-instance
`code_component`, coverage), `src/cli/map.rs` (`figmog map`). Agents
should reuse a project's existing components instead of rebuilding them
from rectangles; the map records which code component implements which
Figma component.

**The file.** `figmog.map.json` at the project root (§16), committed, small
and hand-confirmed (v1 has no `map suggest` and no Code Connect import):

```
{
  "version": 1,
  "components": [
    {
      "figma": {"component_set": "<key>"},          // or {"component": "<key>"}
      "code": {"path": "src/components/PricingCard.tsx", "export": "PricingCard"},
      "props": {                                     // optional
        "Tier":  {"prop": "tier", "values": {"Free": "free", "Pro": "pro"}},
        "Title": {"prop": "title"},
        "Badge": {"prop": "badge"}
      }
    }
  ]
}
```

- **Identity: the Figma key.** An entry names a component (a variant or a
  standalone component) or a component set by its `key` from the file's
  `components`/`componentSets` maps (the `components` and
  `component_sets` sinks, §4), never by node id: the key is what Figma
  publishes, survives renames and moves, and is the same in the library
  and in every file that uses the component, while an instance's
  `componentId` is a file-local node id. A `component` entry wins over its
  set's `component_set` entry. Keys come from `figmog components`.
- `code.path`: relative to the project root, `/`-separated, no `..`, not
  absolute (the same rule as `[stack] tokens`); figmog never reads the
  file, `map coverage` only checks that it exists inside the project (a
  symlink leading out of it reads `outside_project`). `code.export`: a
  JS identifier starting with an uppercase letter (a JSX component).
- `props`: Figma property name → `{prop, values?}`. The name is a
  variant property (`Tier`) or another component property, written with
  or without Figma's `#id` suffix (`Title` matches `Title#11:0` unless
  two properties share the base name — then the full name is needed).
  `prop` is a JSX attribute name (a JS identifier, `-` allowed after the
  first character); two Figma properties can't share one. `values` maps
  the Figma value as text (`"Pro"`, `"true"`; for an instance swap, the
  swapped-in component's key) to the code value — a string, number,
  boolean, or `null` (leave the prop out: the code default). Without
  `values` the Figma value passes through (a string, or a boolean); an
  instance swap without `values` renders the swapped-in component's own
  mapped element (`icon={<StarIcon />}`).
- **Strict.** Unknown keys anywhere, a missing or other `version`, a
  duplicate target, a bad path, export or prop name are errors, each
  naming the file and the JSON path:
  `figmog.map.json: components[0].props["Tier"].prop: "1tier" is not a
  prop name …`. A map key naming no property is not a file error (the
  file is checked without the mirror): `spec` and `coverage` list it
  under `unknown_props`.

**In the spec** (§17). Each INSTANCE whose component or set has an entry
gets `instance.code_component`:

```
{
  "figma": {"component_set": "<key>"},           // the entry that matched
  "path": "src/components/PricingCard.tsx",
  "export": "PricingCard",
  "props": {"badge": true, "tier": "pro", "title": "Pro"},
  "jsx": "<PricingCard badge tier=\"pro\" title=\"Pro\" />",
  "coverage": "full" | "partial",
  "unmapped_properties": [{name, type, value, reason}],   // by name
  "unmapped_overrides": [{id, name, field, reason}],      // by id, field
  "unknown_props": ["<map key>"]                          // sorted
}
```

`jsx` lists the props in name order: a string as `"…"` (or `{"…"}` when it
holds `"`, `\`, `&`, braces, angle brackets or a control character), `true`
as the bare name, other values as `{…}`, a swapped-in element as
`{<Export />}`; `null` props are left out. **`coverage` is `"full"` only
when nothing is listed under `unmapped_*`** — the map never claims an
instance it can't express:

- `unmapped_properties`: an instance property the map has no prop for; a
  value not listed in `values`; an instance swap to a component with no
  entry; a property bound to a variable (the map would pass a literal);
  an ambiguous suffix match.
- A **`component` entry** names one variant, so its identity already
  expresses every VARIANT property (`Tier=Pro` needs no prop); a
  `component_set` entry has to map them. Other properties need a prop
  either way.
- `unmapped_overrides` (`reason` says which kind):
  - `override` — every `overrides[].overriddenFields` entry, on the
    instance and on every nested instance, that a mapped property doesn't
    explain. Explained means the overridden node's
    `componentPropertyReferences` names, for that field (`characters`,
    `visible`, `mainComponent` for `componentId`), a property of this
    instance that the map expressed. Text overrides and style changes
    made by hand are therefore listed; so is every override on a nested
    instance (figmog can't tell whether the main component or this
    instance made it, so it errs towards partial). The instance's own
    `componentProperties` override is covered by the property check.
  - `swap` (field `componentId`) — a nested instance whose component
    differs from the one at the same place in the main component, unless
    its `componentPropertyReferences.mainComponent` names a mapped
    property of this instance. This doesn't rely on Figma listing the
    swap in `overrides`. "The same place" uses Figma's id convention for
    instance children: `I<instance>;<node>` copies main-component node
    `<node>` (`I1:2;10:7` ↔ `10:7`; deeper, `I1:2;10:7;30:1` ↔
    `I10:7;30:1`), checked against the main component's nested instances
    in the mirror (`query::spec` passes them, node id → component id, as
    the component record's `main`). Only nested instances that match are
    descended into.
  - `unverifiable_swap` (field `componentId`) — a nested instance that
    can't be checked: the main component isn't in this mirror (a library
    component; each outermost nested instance is listed), or it has no node at that place. Conservatively partial.
  - `unverified_slot_shape` (field `slot`) — a `SLOT` descendant with
    children. **Slot detection rests on an assumed REST shape** (slots
    as `type: "SLOT"` nodes whose children are the content), not yet
    verified against real Figma files; it also flags default slot
    content. Kept conservative (partial) until verified with real Figma
    access.
- `unknown_props` (map keys naming no property of the instance — a stale
  entry) is a warning only and doesn't make the mapping partial.

**`figmog map check`** validates the file (exit 1 with the file and JSON
path; a missing file is an error) without reading the mirror:
`{"map": "figmog.map.json", "components": <entries>}`.

**`figmog map coverage`** compares the map with the mirror's design-system
inventory (the `figmog_components` read, socket-routed or direct like any
read) and the project's files. A missing map file is an empty map; an
invalid one fails.

```
{
  "map": {"path": "figmog.map.json", "present": bool},
  "summary": {mapped, unmapped, not_in_mirror, code_files_not_present},
  "mapped": [{"figma": {kind, key, name, node_ids}, "code": {path, export},
              "code_file": "present" | "missing" | "outside_project",
              "unknown_props": [...] | null}],      // by kind, then key
  "unmapped": [{kind, key, name, node_id, variants?, variants_mapped?}],
                                                    // by name, key, node id
  "not_in_mirror": [{"figma": {...}, "code": {...}}]   // by kind, then key
}
```

`unmapped` lists each component set that neither has an entry nor has
every variant mapped (with its `variants` and `variants_mapped` counts),
and each standalone component without an entry. `not_in_mirror` lists
entries whose key this mirror doesn't have (a removed component, or one
from another file of the project). `unknown_props` checks the map's
property names against the set's `componentPropertyDefinitions` (for a
set or a variant entry). It is null when there are no definitions to
check against: for a standalone component (`figmog_components` doesn't
report its definitions), and for a set whose node isn't in the mirror (a
library component set used here: `figmog_components` reports its
`property_definitions` as null). Both `map` commands read the project's
map, so neither takes `--db`, and `coverage` reads the project's default
mirror (figmog.toml's `[files] default`, else `.figmog/current`) — the
same one `figmog components` reads; it takes no file argument. Listing code components with
no Figma counterpart (scanning the code), `map suggest` and Code Connect
import come later.

**MCP.** The map reaches agents only through `figmog_spec`
(`code_component`); there is no separate map tool. The stdio relay
injects the project's map per call (an agent's own `map` argument wins).

**Tests.** `src/map.rs` unit tests (every validation error's file and
path, JSX rendering and escaping, partial reasons, nested overrides and
slots, the suffix ambiguity); `tests/map.rs` on the synthetic
`tests/fixtures/map/pricing.json` + `figmog.map.json` (the done-when:
`spec` emits `<PricingCard badge tier="pro" title="Pro" />` for the
covered instance and a partial mapping naming the hand-edited price for
the other, byte-identical across runs and against `figmog call
figmog_spec` with the same `map`; `coverage` reports the unmapped
components, the entry that's not in the mirror, a stale prop and a
missing code file; invalid maps fail `check`, `coverage` and `spec` with
the file and path); `tests/owner.rs` covers the relay's injection.

## 19. Asset pipeline (NWS-2952)

Source: `src/assets.rs` (pure: icon discovery, naming, React components,
content-hashed image names, the manifest), `src/svg.rs` (the SVG
sanitizer), `src/cli/assets.rs` (reads, fetches and writes).

`figmog assets export [--icons <node>]... [--images <node>]... [--force]
[--partial]` exports, into the folders figmog.toml's `[assets]` names
(§16):

- **Icons.** Each `--icons` root yields icons: a component set → each
  visible variant; a page, section, frame or group holding components →
  each visible component set's variants and standalone component at any
  depth (a component's own content is never searched); anything else (a
  component, an instance, a vector, a frame of vectors) → the node itself.
  Each icon is rendered as SVG at scale 1 (the `figmog images` render
  cache, version-pinned, §15), sanitized (below), and written twice:
  `<icons>/svg/<Name>.svg` and a React component `<icons>/<Name>.tsx`
  (or `.jsx`), plus a barrel `<icons>/index.ts` (`index.js`).
- **Images.** Every visible `IMAGE` fill under each `--images` root
  (visible nodes only) is fetched (`figmog_images` with `renders: false`)
  and written to `<images>/<first 16 hex of its SHA-256>.<ext>`, the
  extension sniffed from the bytes (PNG, JPEG, GIF, WebP; anything else
  is skipped). The name depends only on the content, so an unchanged
  image never produces a diff.
- **Manifest** (`[assets] manifest`): `{"$generated", version: 1,
  sources: {file, icons, images}, icons: [{component, figma_name, file,
  key, node_id, path, svg, colors: "single"|"multi", current_color}],
  images: [{file, image_ref, path, url, format, bytes, sha256, nodes}]}`
  (`file`: the Figma file key the export read; `figma_name` cut to 512
  bytes) — sorted keys,
  icons by component name, images by imageRef, no timestamps or file
  version (a re-export after unrelated edits is byte-identical). `url` is
  the root-relative URL when the images folder is `public` or under
  `public/` (the Vite / Next.js static folder), else null. `sources`
  records the file and the roots, so a plain `figmog assets export`
  repeats the last export — and refuses when the project's file is no
  longer the recorded one (the roots are that file's node ids; pass
  `--icons`/`--images` to export from the new file, which replaces the
  manifest). `figmog spec` reads it (§17).

**Naming.** An icon's name is built from ASCII letters and digits only
(every other character separates words), PascalCased: a variant from its
set's name (last `/` segment) and its property values (`Icon` / `Name=Arrow
Right` → `IconArrowRight`), anything else from the last `/` segment of its
own name. The `[assets] prefix` (default `Icon`) is prepended unless the
name already starts with it as a word. Names longer than 64 characters,
names with no ASCII letter or digit, Windows device names (`CON`, `PRN`,
`AUX`, `NUL`, `COM1`–`9`, `LPT1`–`9`, any case, after prefixing), and
names that collide case-insensitively (case-insensitive filesystems) are
skipped — every
icon in a collision, with the node ids involved; nothing is picked
arbitrarily.

**Untrusted input.** Names, ids, imageRefs and markup come from the Figma
file. Figma names never reach generated code (only the manifest, as JSON
data); node ids reach a comment only after a strict character check
(`[A-Za-z0-9:;-]`, ≤ 128); imageRefs must be ASCII alphanumeric; file
names are the validated component name or the content hash (SHA-256 via
`ring`, already compiled in through rustls). Every write target is
checked before anything is written: inside the project, no symlink
anywhere on its path below the project root (the file or any directory;
`--force` doesn't change this), a regular file, and no directory on the
way spelled differently on disk (a case-insensitive filesystem: refused,
naming the spelling on disk). Missing directories are created one at a
time, each checked (never `create_dir_all` through a link). Each file is
written atomically: a new temporary file in the same directory (created
exclusively, `O_NOFOLLOW`), then renamed over the target. A target that
exists only under another case (a renamed icon on a case-insensitive
filesystem) is moved aside and replaced, so the new spelling lands on
disk, and is never reported as `stale`. An existing file is replaced only when figmog wrote
it — a text file starting with figmog's generated header (`// Generated
by \`figmog assets export\`…`, `<!-- … -->` for SVG, a leading
`"$generated"` key for the manifest), an image whose bytes are what its
content-hash name says — or with `--force`. One refusal and nothing is
written. A file whose bytes are already right is not rewritten, so
re-exporting an unchanged file changes no files (not even mtimes).

**SVG sanitizing** (`src/svg.rs`, no XML dependency): a strict,
non-validating reader — elements, quoted attributes, text, CDATA,
comments and PIs; the five predefined entities and numeric references
only; any `<!DOCTYPE`/`<!ENTITY>` rejects the document (no entity
expansion); size (4 MiB), depth (64), element (20 000) and attribute (64)
caps; malformed markup is an error. Then an allowlist: only SVG's shape,
paint-server, clip/mask, text and common filter-primitive elements
survive (everything else — `script`, `style`, `foreignObject`, `a`,
animations, `feImage`, prefixed names — is dropped with its content);
only allowlisted presentation/geometry attributes survive (no `on*`,
`class`, `xml:base`); values containing quotes, `<>&`, backticks,
braces, backslashes or control characters are dropped; `href` must be a
local `#id` (or, on `<image>`, a base64 raster `data:` URI) — and a
`<use>` may reference only an `<image>` or a basic shape (`path`, `rect`,
`circle`, `ellipse`, `line`, `polyline`, `polygon`), never a `<g>`,
`<svg>`, `<symbol>` or another `<use>`, so references can't fan out
(dropped and reported otherwise); `fill`,
`stroke`, `clip-path`, `mask`, `filter` may reference only a local
`url(#id)`, and a reference to an undefined id is dropped; `style` keeps
only `mask-type`, `mix-blend-mode` and `isolation` keywords. Every
removal is reported per icon (`removed` in the command's output). Light
optimization only: comments, PIs, whitespace, empty `<defs>`/`<g>`,
unreferenced ids and attribute-less `<g>` wrappers go; geometry is
verbatim. Limitations: no `<style>` sheets (CSS-styled SVGs lose those
styles), no `<symbol>`/`<marker>`/`<switch>`; meant for Figma's own SVG
export shape.

**Fidelity.** An icon is single-color when exactly one flat paint color
(fill, stroke, stop, flood — normalized, `none` ignored; a shape that no
`fill` applies to counts as its default black; an existing
`currentColor` is a color of its own) appears outside
`<clipPath>`/`<mask>` (their colors are geometry, not paint) and it uses
no gradient, pattern, filter, image or `color` attribute (which would
pin `currentColor`). With `current_color = true` that color becomes
`currentColor` (an implicit black fill is written out as
`fill="currentColor"`), so Tailwind text colors apply; multicolor
icons and logos always keep their colors (`colors` in the manifest). Ids
are renumbered in document order: the `.svg` file prefixes them with the
component name; the component with React's `useId()` (reduced to
`[A-Za-z0-9_-]`, prefixed `figmog`), so two instances on one page never
share a gradient, mask, clip-path or filter id. Components need React 18+
(for `useId`) and the automatic JSX runtime, nothing else; they are
`aria-hidden` unless given `aria-label`/`aria-labelledby`, and spread
their props last (`className`, `width`, … override).

**Reads and routing.** The roots' `figmog_subtree` and the
`figmog_components` inventory (component keys, set names), then the
renders and fills — through a running serve's socket when one answers
(the shared owner holds the store), else from the store opened directly,
like `figmog images`: cache hits cost nothing, misses spend Figma API
budget and need a token. `assets` writes into the project, so it doesn't
take `--db`.

**Failures.** An icon or image that can't be exported (a failed fetch,
an unusable name or SVG, a collision) is listed under `skipped` with the
reason; by default a skipped asset writes nothing (so a good manifest is
never replaced by one missing an entry) and exits 1. `--partial` writes
the rest (the manifest then lists only those) and still exits 1. Files a
previous export listed that this one doesn't produce are reported as
`stale`, never deleted.

**Output.** `{manifest, sources, icons: [{component, node_id, path, svg,
colors, current_color, removed}], images: [...], files: [{path, status:
"written"|"unchanged"}], written, unchanged, skipped, stale}`.

**Tests.** `src/svg.rs` (Figma shapes, hostile markup, entities, limits,
`<use>` targets, colors incl. implicit black and `currentColor`, JSX),
`src/cli/assets.rs` (symlinks, simulated case-insensitive lookups, case
renames, atomic writes), `serve::async_tests` (`renders: false` makes no
render request), the multi-file manifest test in `tests/assets.rs`, `src/assets.rs` (SHA-256 vectors, sniffing, naming,
discovery, plan determinism, collisions, opt-in `currentColor`, manifest
validation); `tests/assets.rs` on the synthetic
`tests/fixtures/assets/icons.json` with seeded renders (the done-when: the
icon set exports to React components — golden in
`tests/fixtures/assets/export/` — and re-exporting changes no bytes and no
mtimes; hostile SVG; foreign files, symlinks, `--force`; all-or-nothing
and `--partial`; `spec` with and without the manifest);
`tests/owner.rs` (export routed through the shared owner, the relay's
manifest injection); `examples/slice/app/test/icons.test.mjs` renders the
golden components with React (react-dom/server) and checks repeated
instances never share an id and every reference resolves inside its own
`<svg>`.

Deferred: deleting stale files (`--prune`), sprites, an icon `size`
prop / Tailwind sizing classes, CSS for image fills' scale modes and
crops in `figmog spec`, raster icons, and a sanitizer for `<style>`.

## 20. `@figmog/react`: dev-server plugin, bridge and targets (NWS-2957)

Source: `packages/react/src/` — `vite.ts` (the plugin), `mark.ts`
(attributes), `bridge.ts` + `relay.ts` (the bridge), `figmog.ts`
(settings and map through the CLI), `client.ts` (browser helper),
`target.ts` (target manifest and runner). The Rust crate gains no
dependency: the browser features live in this JavaScript package, which
reads everything through the `figmog` binary (`figmog settings`, `figmog
map check`/`coverage`, `figmog serve`) and parses no TOML itself.
Sample app and end-to-end test: `examples/dev-plugin/`.

**The plugin** (`import { figmog } from "@figmog/react/vite"`) applies to
the dev server only (`apply: "serve"`): a production build never runs it,
so built output contains no attribute, bridge code, token or the word
figmog (asserted on the built files). At start it runs `figmog settings`
in Vite's root; if that fails (no binary, invalid figmog.toml) the plugin
warns and does nothing else.

**Attributes (optional).** With `[inspector] attributes = true` (or the
plugin's `attributes: true`), each component named in `figmog.map.json`
(`code.path` + `code.export`, after `figmog map check` validates the file)
gets `data-figmog="<component|component_set>:<key>"` — the map entry's
Figma identity — and, when `figmog map coverage` finds exactly one node for
it in the mirror, `data-figmog-node="<node id>"`, on its root element. The
transform runs before JSX compilation (`enforce: "pre"`), parses the module
with Vite's own parser, and finds the export (function declaration, arrow
or function expression, `export { Local as Name }`, `export default
function Name`, `memo(...)`/`forwardRef(...)` wrappers) and every value its
own body returns. It marks only when **every** return is a DOM element
(lowercase JSX tag), `null`, or a conditional of those; a fragment, a
component element (`<Card>` — its props may not reach the DOM), an array or
anything else skips the whole component with a dev-server warning, and so
does a root that already has `data-figmog` (explicit markers win) or an
export two map entries name. Settings, map and node ids are read once,
when the dev server starts (restart it after editing them). Dev source maps are not adjusted for the
inserted text (columns after it on that line shift). Attributes are
optional because one component renders many instances (every Pro and Free
card carries the same attributes), fragments have no single root and
portals render elsewhere: verification targets use explicit locators.

**The bridge.** `POST /__figmog/bridge` on the dev server, body
`{"tool": "<name>", "arguments": {...}}`, answers the tool's MCP
`CallToolResult` verbatim (200; tool errors as `isError` content) or
`{"error"}` with 400 (malformed), 401 (token), 403 (refused), 404 (other
`/__figmog/` path), 405 (not POST), 413 (body over 64 KiB), 415 (not
JSON), 502/503/504 (figmog unreachable, busy — 16 calls in flight —, or
silent past 30 s; 150 s for `figmog_images`). Rules, all checked before
anything is forwarded:

- **Session token.** 32 random bytes (base64url) generated when the dev
  server starts, injected into the dev page only, as
  `window.__FIGMOG_BRIDGE__ = {token, url}`; sent back as `x-figmog-token`
  and compared in constant time.
- **Local only.** The peer address must be loopback (`127.0.0.1`, `::1`,
  `::ffff:127.0.0.1`) and the `Host` header exactly `localhost`,
  `127.0.0.1` or `[::1]` (any port) — so `vite --host` exposes nothing to
  the network and DNS rebinding fails.
- **Same origin.** `Origin` must equal `<scheme>://<Host>` — the page's own
  origin — and `Sec-Fetch-Site`, when sent, `same-origin`. Vite's own CORS
  middleware runs first and allows every localhost origin by default, so
  another local dev server's page (another project) can send a request:
  the bridge refuses it and removes the CORS headers from every answer, so
  no other origin can read one.
- **Allowlist.** The 18 local reads (§6 "Read adapters") plus
  `figmog_images` (renders; `refresh: true` refused). Not `figmog_open`,
  `figmog_sync`, `figmog_files`, the internal write tools, `tools/list`
  or any desktop-proxy tool.
- **This project's files.** An explicit `file`, and a Figma URL naming a
  file in `id`/`under`/`target`, must be one of figmog.toml's `[files]`
  keys; without one, the relay's worktree default applies (§6b).
- **No credentials.** The Figma token stays in figmog's own processes; the
  bridge forwards tool calls and returns their results only.

Allowed calls go to one `figmog serve` relay (§6b) the plugin starts on the
first call, in the project root, with the dev server's environment and the
plugin's `serveArgs` (default none: the same settings an agent's relay
uses); line-delimited JSON-RPC `tools/call` frames over its stdio, matched
by id, notifications ignored, no `initialize`. The relay finds or launches
the repository's background owner, checks the handshake and injects the
worktree default file and `figmog_spec`'s project settings — the bridge
re-implements none of it. A relay that exits fails its pending calls (502)
and is started again on the next call; closing the dev server closes its
stdin, and the owner idles out on its own.

**Targets.** `figmog.targets.json` at the project root:

```json
{"version": 1, "targets": [
  {"name": "pricing-pro", "figma": {"node": "1:2"}, "route": "/",
   "locator": {"testid": "pricing-pro"}}]}
```

Strict: unknown keys, a duplicate or non-`[a-z0-9-]` name, a `route` that
is not a path starting with `/` (no other host), and a `locator` that is not
exactly one of `testid` (→ `[data-testid="…"]`) or `css` are errors naming
the JSON path. `captureTarget` (API) / `figmog-shot target --manifest …
--target … --base-url … --out …` (CLI, `--viewport WxH` repeatable)
captures the target with the screenshot runner at the v1 contract viewports
(1440×900 and 390×844, light) — `<target>-<W>x<H>.png` with its capture
manifest, plus `<target>.target.json` — and fails on zero or several
matches (`SelectorMatchError`, exit 1).

**Tests.** `packages/react/test/mark.test.ts` (export forms, branches,
every skip reason, determinism), `test/plugin.test.ts` (a real Vite dev
server on every interface with a stand-in figmog: attributes from settings
and options, nothing in a production build, and the bridge's refusals —
no/wrong token, other origins, non-loopback Host and peer, non-allowlisted
tools, other projects' files, malformed requests, CORS headers removed —
plus relay restart), `test/target.test.ts` (manifest validation, PNGs,
zero/multiple matches, CLI); `examples/dev-plugin/test/e2e.test.mjs`
against the built figmog binary in a fresh `git init` copy of the sample
app with the synthetic `tests/fixtures/map/pricing.json` mirrored by
`pull --from-file`: attributes in the browser and the skipped fragment,
bridge reads (`figmog_node`, `figmog_spec` with the injected map) through
a real relay and owner, refusals, a PNG of one card, the attribute locator
being ambiguous across instances, a clean production build, and the owner
exiting after the dev server closes.

Deferred: Next.js, the in-page inspector overlay (NWS-2950), wiring targets
into `figmog verify` (NWS-2951), fixture data and non-default states in
targets, and the Playwright browser download as an opt-in step of `figmog
init` / `figmog doctor`.
