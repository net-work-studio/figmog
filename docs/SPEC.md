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
   (`rotate_memtable` on each of `list_keyspace_names()`, followed by
   bounded waits that check poisoning through `persist(Buffer)`);
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
  closed (its worker joined within the remaining drain budget, its published view dropped) while its
  ownership anchor is kept, then compacted. A store that a detached read
  still holds a view of keeps fjall's own lock, so compaction's open fails
  and that store is left untouched (one stderr line). Serve never compacts
  under live views: not after an individual write job, and not during a
  reset-barrier reopen (which releases the store unchanged). A worker still
  blocked at the deadline is detached while retaining its store ownership;
  process exit terminates it, and that store is not compacted.

Compaction is best effort over already-committed data: a failure prints
`figmog: store compaction skipped: …` on stderr and never fails the
command that wrote. `FIGMOG_NO_COMPACT=1` (any value but empty or `0`)
disables it. CLI output is flushed before compaction. Flush/retirement waits
share a 15-second budget and stop after five seconds without progress or
on poisoning. Serve skips compaction after its shutdown deadline.

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

**Pin bumps.** `rotate_memtable` / `sealed_memtable_count` (`Keyspace`) and the
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

By default `figmog serve` is a stdio relay to the repository's shared
background owner (§6b), and this section describes that owner's engine.
`--db`, `--figmog-root` and `--no-socket` run the same engine in the
`figmog serve` process itself, with stdio as one more client.

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
URLs/keys. With no explicit `FILE`, a project serve selects the configured
project default (including the per-checkout current-file fallback); an
explicit `FILE` takes precedence. `--db` bypasses project discovery, and
`--figmog-root` uses only explicit startup files. With no selected startup
file, zero is valid and needs no `FIGMA_TOKEN` up front — the server starts
empty and mirrors files as an agent references them. The first selected
startup file becomes the *default* for any tool call that omits `file`;
with none selected but exactly one file later auto-opened,
that one becomes the effective default (`SessionManager::effective_default_key`)
— two or more auto-opened files with no startup default leaves no
default at all, and an omitted `file` then errors, naming
`figmog_open`/`figmog_files`.

Startup opens stores before serving and queues an initial pull for each
missing mirror when watch is enabled. It never waits for network pulls;
those failures appear in job state while the server stays available.
`--no-watch` still opens the selected startup stores, including the project
default, but queues no startup pulls. Invalid paths, store locks and required
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
  `figmog_where`, `figmog_at`, `figmog_spec` (it joins a subtree with
  the whole variables read, §17), plus `figmog_status` and `figmog_files`
  — runs on a pool of `min(4, available_parallelism)` read workers. Node
  reads always use workers because raw payloads and child lists can be
  arbitrarily large.
  Classification is by measured cost on a synthetic 100k-node mirror: the
  inline point reads take under 0.2 ms; `figmog_components` is linear in sets +
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
  store is reset or reopened under it. The operations behind it are a
  close-and-reopen (`StoreReset::Reopen`, tests only) and a routed `pull
  --fresh` (`StoreReset::Fresh`, below): step 1 is the worker's fetch,
  parse and flatten, and step 4 clears the store's contents (its ownership
  anchor stays) under the same ownership before reopening it empty.
  Fresh apply keeps the read gate closed until publication. Acknowledged
  lifecycle jobs and queued CLI writes carry over to the reopened session.

**CLI writers through serve (NWS-2963):** when its socket answers, `figmog
pull` (plain, `--geometry`, `--from-file`, `--fresh`) and `figmog
import-variables` run as *operations* of serve instead of opening the
store (§10). They are internal socket-only tools — `figmog_internal_pull
{file, db, fresh?, geometry?, from_file?}`, `figmog_internal_import_variables
{file, db, content, name, allow_empty?, dry_run?}` and the receipt read `figmog_internal_operation
{id}` — never listed by `tools/list` and refused on stdio as unknown, so
the agent tool surface is unchanged (agents keep `figmog_open`/`figmog_sync`).

- *Operation ids.* Each accepted write gets id `<generation>-<seq>`: the
  generation is this process's pid and start time (hex), the sequence
  counts from 1. The requester's connection first receives
  `{"method": "notifications/figmog/operation", "params": {"id": ...}}`;
  the `tools/call` is a deferred response (NWS-2955) sent only once the
  write committed **and** its view was published — `{"operation": id,
  "result": <churn | {"imported","removed"}>}` — or a `-32000` error if it
  failed, before changing the store. A failed reopen/apply after a fresh clear is
  reported as outcome unknown (`-32002`), since the previous mirror may already have
  been cleared. The wait is the requester's own
  connection; the loop never blocks on it.
- *Receipts.* A finished operation's outcome is kept for 10 minutes;
  `figmog_internal_operation {id}` (any connection) returns `{"id",
  "state": "succeeded", "result"}`, `{"id", "state": "failed", "error"}`,
  `{"id", "state": "running"}`, or an unknown-operation error (another
  process's id, or expired). This is a process-scoped management query:
  its ID is sufficient even when the checkout has no default or current
  file. Receipts and ids are in memory only.
- *Ordering and coalescing.* Writes run on the file's worker (still its
  only writer) in arrival order, ahead of queued watch/sync jobs, and,
  like a direct pull, without waiting out a failed pull's backoff. Two
  writes are never merged, except identical plain pulls (network, not
  fresh, same `geometry`) that have not started yet *and are adjacent in
  the queue* — a plain pull joins only the last queued write, never one
  with another write (an import, a saved-file, fresh or differently
  configured pull) queued after it: they share one fetch and apply, each
  keeping its own operation id. A network write uses
  serve's credentials, read at startup. Restart it after changing credentials,
  or use `--no-socket` to use the invoking CLI's credentials. While a write is pending on a not-yet-mirrored
  store, reads queue no implicit first pull. A successful routed pull
  updates `last_success` (`"job_id": null`, plus `"operation"`) and clears
  the failed-pull watch deferral while preserving independent metadata
  Retry-After and ordinary cadence, including across a fresh store reopen.
  Pending expedited observations and owed polls also survive the reopen.
  A routed pull is not a lifecycle job.
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
`figmog_spec` (the binding-aware spec, §17) + `figmog_images` (§15) + 2 multi-file management tools that aren't
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
Local `arguments` must be an object or `null` (the latter means no
arguments). Read arguments are validated before opening or pulling a
mirror. Floating-point representations of `depth` and search `limit`
remain rejected with `got floating-point number <value>`; point lookup's
`limit` retains its non-negative integral-number rule (§10). CLI point
coordinates must be finite so direct and socket serialization agree.

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
loop reads. Ordinary frames are capped at 8 MiB. A socket-only
`notifications/figmog/write-frame` prelude grants exactly the following
hidden routed pull/import frame a 256 MiB cap; other methods cannot use
that allowance. The CLI sends the prelude for large saved-file or variable
imports and refuses writes above 256 MiB before sending. Oversized frames
drop the connection. Output is bounded separately (below). `initialize` is optional for socket clients: `tools/call` is
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

SIGTERM and SIGINT enter the same graceful drain as idle shutdown. Close
attached relays before terminating an owner, since a connected relay can
restart a lost owner.

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

**Socket.** `<runtime directory>/<16 hex>.sock`, the hex a 64-bit FNV-1a
hash of the project identity. A valid user-owned mode-0700
`$XDG_RUNTIME_DIR` is preferred, with a private `figmog` child; otherwise
`/tmp/figmog-<uid>` is used. Non-UTF-8 paths and paths that would reach
macOS's 104-byte limit fall back to the short `/tmp` address, so discovery
can represent the exact socket path in JSON. Directories and sockets are checked
for ownership, modes and symlinks before connecting, and the connected
peer must have the current uid. Discovery must name the expected socket.
An unsafe or pre-planted runtime directory produces an actionable error;
figmog does not connect through it. The socket is 0600. Under the lock, a file already at the address is a
leftover of this project's previous owner: it is removed only if a connect
to it fails and it is a socket owned by the user (a live listener or a
foreign file is an error). On exit the socket is unlinked only if it is
still the inode this owner bound. An owner whose socket disappears or is
replaced (checked every second) shuts down rather than run unreachable.

**Discovery.** Once it can answer, the owner writes `<mirror
root>/owner.json` (0600; temp file, fsync, rename):
`{"generation","pid","project","protocol","socket","version"}`. The
generation is the owner's pid and start time (hex), also the prefix of its
operation ids (§6). Clients validate the recorded project-specific basename and private runtime
root independently of their own XDG environment before connecting; clients
using incompatible runtime namespaces fail
with a lock-held/socket-unreachable diagnostic rather than relaunching
losing owners repeatedly. Detached owners mark inherited descriptors
close-on-exec, leaving only their configured stdio open. The owner
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
management calls pass through unchanged. Typed read and management arguments
are validated before selecting a default, checking migration state, or recording
a referenced file. A rejected request therefore cannot change the relay's
sole-file fallback. Unknown tool names pass through without changing it.
Every request a relay forwards
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
and starts reconnecting at once while stdin remains open. Runtime reconnects
run off the event loop; queued requests wait for a connection without delaying
EOF handling. On every new connection the relay repeats the
handshake (with its startup files) and the agent's `initialize` (under a
relay-internal id whose answer it swallows), so notifications keep
flowing.

**Agent output and EOF.** Relay responses use a dedicated stdout writer,
so a client that stops reading cannot block the event loop from receiving
stdin EOF. Writes to the owner are queued off the event loop as well. Pending
replies, reconnects and the final output flush share the original 15-second
exit-drain deadline. At expiry, connection work is cancelled, including a
blocked connect or handshake; a late connection cannot attach, initialize, or
resend requests. No further owner launch is started after cancellation. The
owner connection closes before the final flush, so blocked output cannot keep
the owner attached. Initial startup retains its separate 60-second budget.

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
legacy snapshot — plus the variable's sorted, deduplicated `scopes`, its
existing `collection_id`, and its collection's `default_mode` (by name,
the mode id when unnamed; NWS-2967). Without
`--snapshot`/`snapshot: true` the result stays the plain row array; with
it, `{snapshot, variables}`.

### Token export (NWS-2967)

`figmog tokens export [--out <path>] [--collection <name>] [--light-mode
<mode>] [--dark-mode <mode>]` renders the variable snapshot as one CSS
file (`src/tokens.rs`, pure; the CLI half is `src/cli/tokens.rs`).

**How it reads.** It is not a new read tool: the CLI maps it to the
existing `figmog_vars {snapshot: true}` read (`ReadCommand::Vars`), so it
is socket-routed to a running serve/background owner or opens the store
directly exactly like `figmog vars --snapshot` (§10 "Socket-first
routing"), and renders that JSON locally. A serving binary older than the
`default_mode` or `collection_id` fields is reported as an error asking
to restart it.

**Snapshot.** Only state `current` exports; `none`, `legacy` (no
provenance) and `empty` are exit-1 errors. Only authoritative rows are
exported — an inferred-only variable (bound in the file, absent from the
snapshot) is skipped and reported.

**Output.** Deterministic, every block sorted by property name:

1. a header comment with the snapshot's `source`, `format`,
   `format_version`, `exported_at` and `digest` (null when absent) and the
   theme choice;
2. `:root { … }` — every exported variable's value in its collection's
   default mode, except the theme collection, which uses its light mode;
3. `[data-theme="dark"] { … }` — the theme collection's variables in the
   dark mode (absent without a theme collection). Every theme-collection
   variable, aliases included, is redeclared there, so `data-theme` works
   on the root or on any subtree. A custom property's `var()` resolves
   where the property is declared, so every variable outside the theme
   collection that aliases a theme variable — directly or through a chain
   — is also redeclared there, with its one value (otherwise
   `var(--color-action-primary)` would keep its light value inside a dark
   subtree);
4. `@theme { … }` and `@theme inline { … }` — the Tailwind v4 theme
   (below). Empty blocks are omitted.

**Theme collection and modes.** Collections are grouped and tested for
theme membership by ID. `--collection` selects a display name and rejects
that name when more than one collection has it. Without `--collection`,
the theme is the one collection with a mode whose name contains "dark"
(ASCII-case-insensitive) — none ⇒ no dark block (an error if
`--light-mode`/`--dark-mode` was given), several ⇒ an error asking for
`--collection`. The dark mode is `--dark-mode`, else the collection's one
mode named like "dark"; the light mode is `--light-mode`, else its one mode
named like "light", else its default mode. Unknown names, several
matches and light = dark are errors. Other mode kinds and combining
collections are out of scope.

**Names.** `codeSyntax.WEB` (`--x` or `var(--x)`) when `x` is nonempty
ASCII letters, digits, `-` and `_`; an invalid WEB value is reported and
falls back to the name path: the variable name lower-cased, every run of
non-alphanumerics (including the `/` group separators) one `-`, prefixed
`--` (`Color/Brand Primary` → `--color-brand-primary`; no collection
prefix). A name with no ASCII alphanumerics is skipped and reported. Two
variables with one name are an error naming both (`token name collision:
`a/b` (VariableID:…) and `a b` (VariableID:…) both export as `--a-b` …`).

**Values.** COLOR → `#rrggbb`, or `#rrggbbaa` below full alpha (each
channel `round(clamp(x, 0, 1) × 255)`). FLOAT → `<n>px` when its `scopes`
include `GAP` or `WIDTH_HEIGHT` (spacing/size) or `CORNER_RADIUS`
(radius); any other FLOAT is emitted unitless and reported. Numbers print
in shortest round-trip form (`16`, `0.5`). An alias → `var(--target)`;
CSS resolves chains. Every emitted FLOAT carries its own unit, so a px
FLOAT aliasing a unitless one → `calc(var(--target) * 1px)` (a bare
`var()` would make `padding: 16` invalid, i.e. 0px), while a unitless
FLOAT aliasing a px one is skipped and reported (CSS cannot strip a
unit; emitting the resolved number would silently detach the alias), as
is an alias between COLOR and FLOAT. STRING, BOOLEAN and other types are skipped and
reported, as is a variable missing a needed mode's value or aliasing a
variable that isn't exported (transitively).

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
  `var(--color-brand)`, which the unlayered `:root`/`[data-theme]` blocks
  override per mode.

A theme name that collides with another theme name or another token's
name is an error naming both variables.

**Output and report.** Without `--out` the CSS goes to stdout and, when
anything was reported, one `{"report": [...]}` JSON line to stderr. With
`--out` the file is written and stdout gets `{out, tokens, theme,
report}` JSON. Report entries are `{variable_id, name, reason}`, sorted.

**Fixtures.** `tests/fixtures/tokens/variables.json` (synthetic: a
Light/Dark `Theme` collection, an alias chain `action/hover →
action/primary → brand → ocean/500` and a GAP alias of a unitless FLOAT)
exports to the golden `tests/fixtures/tokens/export.css`
(`tests/tokens.rs`; `FIGMOG_BLESS=1` regenerates it).
`packages/react/test/tokens.test.ts` compiles that golden file with
Tailwind v4 and asserts computed values in Chromium in both modes.

## 10. CLI surface

JSON is the CLI's only output mode (there is no `--json` flag — there's
nothing to select between). Every command prints pretty-printed JSON on
stdout via one seam (`cli::write_json`) — the one exception is `tokens
export` without `--out`, whose stdout is the CSS file itself (§9); every failure prints
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
`src/cli/migrate.rs` (`migrate`, §16), and `src/cli/tokens.rs` (`tokens
export`, §9 — mapped onto the `figmog_vars` read, then rendered as CSS).
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
`figmog.local.toml` fails every such command with `<file>:<line>: <message>`.
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

**Writers (NWS-2963).** `pull` (including `--fresh`, `--from-file`,
`--geometry`) and `import-variables` resolve their mirror exactly as
direct mode does, then try the same socket. Reachable ⇒ they run as serve
operations (§6 "CLI writers through serve"): a `--from-file` response is
read, parsed and flattened for validation in the invoking worktree
(the direct path's errors) before any destination session is opened, then
sent as content; an import is read and fully
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

## 16. Project config and mirror roots

Source: `src/project.rs` (discovery, config, active-store rules),
`src/cli/migrate.rs` (`figmog migrate`).

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
relative path resolved against that directory. If an ancestor has a `.git`
entry, Git invocation/refusal/decoding errors stop the command, including
Git's stderr; paths are decoded as OS bytes. Outside Git the mirror root
is the local root, `<project>/.figmog/`. An explicitly group-shared Git
common directory uses private per-checkout mirrors instead, preserving the
ancestor ownership and write-permission checks.

**Config.** `figmog.toml` and an optional `figmog.local.toml` beside it
(machine-specific; `pull` adds it to `.gitignore` alongside `.figmog/` when
the project has a `figmog.toml`). Both use the same hand-parsed TOML subset
(no `toml` dependency):

```toml
[files]
default = "AbCdEf1234"                                  # file key or Figma URL
design-system = "https://www.figma.com/design/XYZ/..."  # named entry
```

Supported: an optional UTF-8 BOM, `#` comments, blank lines, quoted or bare
`[table]` headers (including `["files"]`), and inside
`[files]` only `key = "string"` (bare or quoted keys; basic strings with
`\" \\ \n \t \r \uXXXX` escapes, or literal `'...'` strings). Every
`[files]` value must be a file key or Figma URL and is stored as its key.
Unsupported `files` encodings (dotted assignments, inline tables, array
tables or subtables) are explicit errors. Other tables, top-level keys and
`[[array]]` tables are skipped unparsed
(reserved for later settings, never an error), except that multi-line
strings (`"""` and `'''`) in skipped values are tracked so a `[files]`
line inside one is content, not the table; multi-line values inside
`[files]` itself are an error. Errors — unterminated
headers, strings or multi-line strings, a missing `=`, a non-string value, trailing text, an
invalid file reference, a duplicate key or a second `[files]` table — read
`figmog.toml:<line>: <message>` (or `figmog.local.toml:...`). Local entries
replace same-named ones. Only `default` has meaning today: it is the file
every command uses when none is given, ahead of `.figmog/current`, which
remains the fallback (and is still written by `pull` and serve's default
session). Named entries are parsed and validated but not yet addressable.

**Legacy roots.** Every existing `.figmog` directory between the working
directory and project root is checked, nearest first for `current`. A
successful pull publishes the project-root pointer and retires preceding
nested pointers, so a stale legacy default cannot mask the newly pulled
file. In-process serve uses the same publication rule for its default
session. Old mirrors can be read and migrated from any of those roots. Multiple legacy
copies of one key are an explicit ambiguity error naming their paths.

**Active store.** The mirror root is where mirrors *should* live; the
active store is where a key's mirror *is*. Outside Git both are the same
directory. Inside Git, for each key:

| `.figmog/<key>/db` (old) | `<mirror root>/<key>/db` (shared) | result |
|---|---|---|
| exists | absent | use the old store; stderr `{"warning": "migration pending: ..."}` |
| absent | exists | use the shared store |
| absent | absent | writes may create a shared mirror; reads report no mirror |
| exists | exists | error naming both paths (never a silent pick) |

With both present and a `migrated-from` marker in the shared `<key>/`,
the error instead says a `figmog migrate` was interrupted and to rerun it.
The warning is printed by every direct CLI command that resolves the old
store and by serve when a session opens it; JSON outputs are unchanged.
The old store is resolved before its lock is taken, so opening it never
creates it: once its lock is held it must still exist and still be the
active store, otherwise (a `figmog migrate` moved it meanwhile) the
command fails asking for a rerun, which resolves the shared store. A
missing old store is never recreated. Read-only CLI commands, whether
direct or routed over a control socket, never initialize a missing store.
MCP first references over stdio or an owner relay may open a new session
and queue its initial asynchronous pull (§6). Routed writes include the
intended database path; a serve configured for another database refuses without applying
anything and points to `--no-socket`. New mirrors go to the selected mirror
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
`{"mirror_root", "migrated": [{"file", "from", "to", "resumed"}],
"failed": [{"file", "error"}]}`. Every key is attempted, both lists are
sorted, and any failure produces exit 1 after printing the complete report.
When mirror and legacy roots coincide it is a no-op. Per key:

0. Take the repository-wide migration lock
   `<mirror root>/<key>.migrate.lock` (every worktree shares the
   destination and staging dir), held until the end; while another
   migration of the key holds it — from any worktree — this one is
   refused with a message to rerun once it finishes.
1. Refuse if `<mirror root>/<key>` already exists.
2. Open the old store with its ownership anchor
   *and* fjall's own writer lock, so a running serve (even an older binary)
   fails this with the standard "store is locked" error — then close the
   database to quiesce its workers, then acquire and retain the same native
   file lock fjall uses on `db/lock`, alongside the anchor. Revalidate the
   directory and lock identities; if an older writer wins this transition,
   fail before copying or deleting anything.
3. Remove any staging dir `<mirror root>/.<key>.migrating` left by an
   interrupted run (safe: the migration lock excludes a live one), copy the old directory into it (regular files and
   directories only, fsynced; the lock anchor is not copied), take a fresh
   anchor and native writer lock on the staged `db/`, and write
   `migrated-from` naming the source. Revalidate source and staged ownership
   before publication.
4. Atomically rename the staging dir to `<mirror root>/<key>`; the staged
   anchor and writer lock (still held) move with it, so the destination is
   owned the moment it appears.
5. Atomically rename the old directory to its tombstone
   `.figmog/.<key>.deleting` beside it, retaining both locks. Revalidate the
   renamed tombstone and remove it recursively, then the marker. Never revisit
   the old source pathname: an older process may have created a new store
   there after the rename. A tombstone is never opened as a store.

A crash during 3 leaves the old store intact and active (the staging dir
is ignored and redone on rerun). A crash after 4 leaves the marker, which
the active-store rules report and a rerun finishes by doing 5 while holding
both stores' anchors *and* native fjall writer locks (as in 2, so an older
binary's serve on either store refuses it). The marker names the source
checkout's old directory, and only a rerun from that checkout removes it:
another worktree's own old mirror is an independent copy, so there the
rerun refuses, naming the checkout that must finish — unless the recorded
source is already gone, when it just removes the source's tombstone (if
any) and the marker. A crash during 5's deletion leaves a half-deleted
tombstone, which a rerun removes without opening it. An old directory an
older binary's interrupted step 5 left half deleted in place is still
protected by anchors and native writer locks without opening or recovering
its contents. The same protection applies while removing stale staging
directories and tombstones; missing lock files inside these partial private
trees are created without initializing a database. A stray tombstone with no
marker is removed by any rerun.

## 17. Binding-aware spec (NWS-2968)

`figmog spec <node>` and the `figmog_spec {id}` tool describe one
auto-layout subtree — typically a card — the way a developer rebuilds it:
layout plus values, every value either the variable token it is bound to
or explicitly untokenized. Source: `src/spec.rs` (pure), composed in
`query::spec`.

**How it reads.** A local read like any other (`ReadCommand::Spec`, §6
"Read adapters"): direct CLI, socket/owner-routed CLI, `figmog call` and
MCP all return the same JSON, and the NWS-2902 parity tests cover it. It
runs on a read worker. The inputs are exactly the `figmog_subtree {id}`
answer (raw JSON, children nested) and the `figmog_vars {snapshot: true}`
answer, so the output is a function of the mirror alone. `id` takes a
bare id, a `12-34` id or a Figma URL (§12); an unknown id is the standard
`no node … in the mirror` error.

**Output.** One JSON object, every key sorted (`serde_json` without
`preserve_order`):

```
{
  "node": <spec node>,
  "tokens_snapshot": {state, source, format, format_version, exported_at,
                      digest, variables, collections, theme, css_unavailable},
  "unsupported": [{"id", "reason"}, ...]   // sorted by id, then reason
}
```

A spec node is `{id, name, type, layout?, values, children}` with
`children` in child-index order. `tokens_snapshot` is the §9 snapshot
summary the names refer to, plus the theme the token export picks
(`{collection, collection_id, light_mode, dark_mode}` or null) and
`css_unavailable`:
null, or the token export's error (no `current` snapshot, a name
collision, an ambiguous theme), in which case every `css` is null.

**Layout** (only on auto-layout frames and on children of one):

- `direction`: `horizontal` | `vertical` (`layoutMode`);
- `gap`: a value entry for `itemSpacing`, or `"auto"` under
  `SPACE_BETWEEN`;
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
  hug; else fixed. Emitted on children of an auto-layout parent, on
  auto-layout frames, and on the spec root when it carries
  `layoutSizing*`;
- `size`: `{width, height}` from `absoluteBoundingBox`, whenever `layout`
  is present.

**Values:** `fill` and `stroke` (arrays, one entry per visible `SOLID`
paint; the raw value is `#rrggbb`, or `#rrggbbaa` below full alpha, the
paint's `opacity` folded into alpha — the token export's color form),
`stroke_weight` (with visible uniform strokes, defaulting to 1 when
`strokeWeight` is omitted), `radius` (`{top_left, top_right,
bottom_right, bottom_left}` from `rectangleCornerRadii`, else
`cornerRadius`; absent when the node has neither and no corner binding),
and on TEXT nodes `typography`: `font_family`, `font_size`,
`font_weight`, `line_height` (`lineHeightPx`; `"auto"` for
`INTRINSIC_%`), `letter_spacing`, each present when the type style has it.

**Value entries.** Bound:
`{"token": <variable name | null>, "variable_id", "css": <name | null>,
"raw": <Figma's baked value>}`. Bindings are read from the paint's
`boundVariables.color`, else the node's `boundVariables.fills[i]` /
`strokes[i]`; `itemSpacing`, `paddingTop`…, `strokeWeight`,
`topLeftRadius`…; text properties from the node's `boundVariables`, else
its type style's (`fontFamily`, `fontSize`, `fontWeight`, `lineHeight`,
`letterSpacing`), an alias or an array of aliases naming one variable.
`token` is the variable's name in the snapshot (null for a variable bound
in the file but absent from the snapshot); `css` is the custom property
name `figmog tokens export` emits for it (§9 "Token export", taken from
that export run on the same snapshot, so they cannot drift) — null for a
variable the export skips (STRING, BOOLEAN…). An alias variable is shown
as itself (`surface`, not the primitive it aliases).

Unbound: `{"untokenized": true, "raw", "matches"?}` — never token usage.
`matches` lists (sorted by token name, then id) every snapshot variable
`{token, variable_id, css}` whose value in its root mode — the theme
collection's light mode, else its collection's default mode, aliases
followed — equals the raw value (colors as `#rrggbb[aa]`, numbers within
1e-6, strings exactly), of the property's type, and scoped for it:
`ALL_SCOPES`, or `ALL_FILLS` plus `TEXT_FILL` / `FRAME_FILL` /
`SHAPE_FILL` by node type for fills, `STROKE_COLOR`, `STROKE_FLOAT`,
`CORNER_RADIUS`, `GAP` (gap and padding), `FONT_FAMILY`, `FONT_SIZE`,
`FONT_WEIGHT`, `LINE_HEIGHT`, `LETTER_SPACING`. Omitted when nothing
matches.

**Unsupported, never approximated.** Each case is listed under
`unsupported` with the node id and reason, and the affected part is left
out of the spec:

| case | left out |
|---|---|
| `visible: false` | the node's layout, values and children |
| `layoutMode` other than `HORIZONTAL`/`VERTICAL`/`NONE` (e.g. `GRID`) | its own layout; children get no sizing |
| `layoutWrap: WRAP` | its own layout; children get no sizing |
| `layoutMode: NONE` with children (constraints-based frame, group) | children's positions (no sizing) |
| `layoutPositioning: ABSOLUTE` | the node's position and sizing |
| TEXT with nonzero `characterStyleOverrides` (mixed runs) | its fill and typography |
| a visible non-`SOLID` paint (gradients, `IMAGE`, …) | that paint |
| a property bound to several variables (mixed alias array) | that property |
| visible `effects` | the effects |
| layer `opacity` below 1 | the opacity |
| `individualStrokeWeights` | the stroke weight |

The node's other values and its children are still specified.

**Fixtures.** `tests/fixtures/spec/card.json` (a synthetic file: a
vertical auto-layout card with a horizontal row using the older sizing
fields, a GRID child, an absolutely positioned ribbon with a gradient,
and a mixed-run caption) and `tests/fixtures/spec/variables.json` (a
synthetic export with a Light/Dark theme collection) produce the golden
`tests/fixtures/spec/card.spec.json` (`tests/spec.rs`; `FIGMOG_BLESS=1`
regenerates it). The tests also check that every binding in the
specified nodes appears as its token, that each `css` name appears in
the `tokens export` output for the same snapshot, the flagged unbound
padding, the `matches` gap, and the unsupported grid child.
