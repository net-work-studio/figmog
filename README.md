<p align="center">
  <img src="docs/assets/figmog.png" width="100%" alt="figmog">
</p>

# figmog

figmog is a superset of the Figma MCP surface: a local, instantly-queryable
mirror of your Figma files built for high-performance design agents.

One node read: 0.055ms from the mirror, 972.6ms through Figma's REST API in
the same run. That is 17,657x faster. figmog sustained 149 requests a second
where Figma's Tier-1 file budget is about 10 requests a minute, and
re-syncing a file nobody had edited cost 4.6ms and wrote nothing. Measured
2026-08-17 against a real 5,339-node production file; the full run is
[further down](#the-benchmark-run).

## Quick start

### 1. Get the binary

```console
$ tar xzf figmog-<version>-<target>.tar.gz
$ chmod +x figmog
$ xattr -d com.apple.quarantine ./figmog   # macOS only, see Install
```

Tarballs live on the
[releases page](https://github.com/net-work-studio/figmog/releases).

Check the installed version with `figmog version` (JSON output) or
`figmog --version` / `figmog -V` (plain text).

### 2. Save a Figma token

```console
$ figmog auth login
```

Make one at figma.com under settings, security, personal access tokens,
then paste it into the hidden prompt. figmog saves it in macOS Keychain
or Linux Secret Service and reuses it across terminals, projects, and Git
worktrees. Nothing is written to your project or mirror directory.

```console
$ figmog auth status       # credential source, never the token
$ figmog auth logout       # remove the saved token
```

`FIGMA_TOKEN` still overrides the saved token for CI or another account.
Unset it to use your saved credential; an explicitly empty `FIGMA_TOKEN`
disables saved credentials for offline use. Logout removes only the saved
credential, so an environment override remains active. The shared
background figmog (below) picks up a changed credential when it restarts:
it exits on its own once no agent or command has used it for 10 minutes,
and the next `figmog serve` starts a fresh one. To apply a change sooner,
close every agent session in the repository and wait for it to exit, or
stop it with `kill <pid>` (the pid is in `.git/figmog/owner.json`).

For a password manager or other noninteractive secret source, pipe its
output to `figmog auth login --token-stdin`. The token is never accepted
as a command-line argument. Login stores the token without making a Figma
request; `auth status` reports configuration, not whether Figma accepts it.
Expired or revoked personal tokens must be replaced with `auth login`.
On Linux, saved credentials require a running, unlocked Secret Service
(such as GNOME Keyring or KWallet); headless environments can use
`FIGMA_TOKEN` instead.

### 3. Connect it to Claude Code

```console
$ claude mcp add figmog -- $PWD/figmog serve
```

### 3b. Remove the official Figma MCP server, if you have one connected

```console
$ claude mcp list        # find its name; it is usually `figma`
$ claude mcp remove figma
```

Run one or the other, not both. figmog forwards the tools of Figma's
**desktop** MCP server (uncached, verbatim) when the Figma desktop app is
running with Dev Mode MCP enabled. Open or quit the desktop app whenever
you like: figmog re-checks for it (about every 30 seconds while it is
missing) and tells your agent when its tools appear or go away. Only the
desktop server's tools are forwarded — Figma's remote MCP server is not.
Keep both connections and the agent sees two overlapping Figma
servers, then picks tools badly.

### 4. Ask for something

> Using figmog, pull this file: [figma file URL], then build this frame in
> HTML & CSS: [figma frame URL]

figmog mirrors the file the first time the agent references it, and every
read after that answers from local storage. Frame URLs carry `?node-id=…`,
which figmog's tools accept directly, so paste the URL straight out of
Figma.

## The CLI, for humans

```console
$ figmog pull "https://www.figma.com/design/<key>/<name>"
$ figmog search "pricing card"
$ figmog dump 1:23 --fields id,name,type,fills --depth 3
$ figmog serve                     # the MCP server, in another terminal
```

The first `pull` writes the file key to `.figmog/current`, so later commands
can drop the file argument. Read commands never touch the network and print
JSON on stdout; failures print `{"error": ...}` on stderr and exit 1. Every
command is documented in [`docs/SPEC.md`](docs/SPEC.md) §10.

### Where mirrors live, and `figmog.toml`

figmog works per *project*: the nearest directory with a `figmog.toml`
(searching up from the current directory, but not past the Git top level),
else the Git top level, else the current directory. A project can name the
Figma file(s) it mirrors:

```toml
# figmog.toml — commit this
[files]
default = "https://www.figma.com/design/<key>/<name>"   # or a bare file key
# design-system = "<key-or-url>"                        # named entries (reserved)
```

`default` is the file every command uses when you don't pass one; without
it, `.figmog/current` (the last pulled file) still applies. A
`figmog.local.toml` beside it, in the same format, overrides entries for
your machine only. Other tables and keys are ignored, so later settings
won't break older binaries. A malformed file is an error naming the file
and line (`figmog.toml:3: ...`).

Inside Git, mirrors live in the repository's Git directory,
`<git-common-dir>/figmog/<key>/db` (for example `.git/figmog/`), shared by
every worktree of the repository, so a new worktree needs no re-pull. Git
never tracks that directory. Outside Git, mirrors live in the project's
`.figmog/<key>/db`. The per-checkout `.figmog/` still holds `current`.

Mirrors that earlier versions put in a checkout's `.figmog/<key>/db` keep
working in place, with a "migration pending" warning on stderr. Run
`figmog migrate` to move them into the shared root. It holds both store locks
(so it fails cleanly while `figmog serve` has the mirror open), never
overwrites an existing mirror, and can be rerun to finish an interrupted
move. If a file has a mirror in both places, figmog stops with an error
naming both paths rather than picking one.

After a successful `pull` inside a Git repository, figmog adds `.figmog/`
(and, in a project with a `figmog.toml`, `figmog.local.toml`) to the
closest existing `.gitignore` between the project root and the repository
root, or creates the root `.gitignore` if none exists. Existing ignore
rules are respected. Custom `--db` locations are left to you. A failure to
update `.gitignore` prints a warning and keeps the successful pull result.
Commit that ignore rule. Your saved credential is shared independently
through the system credential store.

## figmog against the API

| | figmog | Figma REST API and MCP |
|---|---|---|
| node read, p50 | 0.055ms | 972.6ms |
| sustained rate | 149 req/s | ~10 file requests/min on the free plan |
| second read of the same node | free | spends the budget again |
| counts, pointer matches, every text node | one local scan | no endpoint offers them |
| re-sync of an unedited file | 4.6ms, zero churn | one Tier-1 fetch |

Three things spend Figma's budget: a pull (`figmog pull`, the poll loop's own
sync, `figmog_sync`, `figmog_open`), an explicit `figmog images` fetch, and a
proxied call that reaches the desktop app. Every `figmog_*` read tool is
free.

## What the mirror gives an agent

### One background figmog per repository

Every agent's `figmog serve` is a thin stdio relay. The first one starts a
detached *background figmog* for the repository, which opens the mirrors,
watches Figma and pulls each changed file once, for every worktree and every
agent at the same time. Each further `figmog serve`, in any worktree,
connects to that same process, so ten agents in three worktrees cost one
poll loop and one pull per edit, with no lock errors. Closing an agent's
session never affects the others. The background figmog exits on its own
after 10 minutes with no clients, and the next `figmog serve` starts it
again, which costs one reconciliation pull per file. Agent setup stays
`figmog serve`. Its log is `.git/figmog/owner.log` (outside Git,
`.figmog/owner.log`).

Each relay keeps its worktree's context: calls without a `file` go to that
worktree's `figmog.toml` default (or the relay's own startup file), never
to another worktree's. Watch, interval and desktop settings come from the
`figmog serve` that started the background figmog; a later one asking for
different settings prints a note on stderr saying which were not applied.
Two repositories get two independent background processes. A relay and a
background figmog from different figmog versions refuse to talk and say
how to let the old one exit. See `docs/SPEC.md` §6b.

### Reads work while the server runs

The background figmog owns the stores and listens on a unix socket under
`/tmp/figmog-<uid>/`, published in `.git/figmog/owner.json`. Read commands,
`figmog tools` and `figmog call` connect there first and answer from its
live view; with no background figmog running they open the store directly.
`--no-socket` forces direct mode (and makes `figmog serve` serve in its own
process, without the shared background figmog). The writers go through the server too: `figmog pull` (with
`--fresh`, `--from-file` or `--geometry`) and `figmog import-variables` become
operations of the running server, and return only once the change is
committed and visible, with the same output as without a server. Files are
read in your checkout and sent as content; an invalid file is rejected before
anything changes. If the server dies mid-operation the command says "outcome
unknown" with the operation id and does not retry. For agents, open/sync on
the server acknowledge a background
job immediately; check `figmog call figmog_status` (or `figmog_files`) for
`ready`, job state, last successful version/churn, and errors. Reads continue
using the last completed mirror while a pull runs. Direct sync without a
running server remains synchronous. See `docs/SPEC.md` §6 and §10.

### One process, many files

`figmog serve [FILE]...` takes zero or more file URLs or keys. Every
`figmog_*` tool has an optional `file` argument, and a file referenced for
the first time queues a pull. Until it completes, data reads report not-ready;
status and files stay available. Starting with zero files
needs no token up front. `figmog_open` and `figmog_files` manage the set. See
`docs/SPEC.md` §6.

### Reads shaped for building

`figmog dump <id>` returns a node and its descendants as nested raw JSON,
with `--depth N` and `--fields a,b,c` to keep the payload small.
`--under <id>` scopes `text`, `find`, `search` and `where` to one subtree,
and composes with `--page`. `--resolve-vars` annotates every
`boundVariables` binding site with the variable's name and per-mode values
under a `resolved_variables` key, so a generator can emit
`var(--color-bg-primary)` instead of a literal. Node arguments accept a bare
id, a `12-34` id, or a pasted Figma URL. See `docs/SPEC.md` §12 and §13.

### Vector geometry

`figmog pull --geometry` asks Figma for `fillGeometry` and `strokeGeometry`
path data, so vector nodes carry their outlines in `raw`. The flag is stored
per mirror, so watch ticks, `figmog_sync` and auto-opens keep requesting
geometry and the records never churn from flag drift. `pull --fresh` turns it
back off. See `docs/SPEC.md` §14.

### Image bytes

`figmog images <node-id>...` downloads node renders plus the image fills
those nodes reference, writes them to `--out` (default `./figmog-images/`),
and prints a manifest. Bytes are cached against the file version, so asking
again for an unedited file spends nothing. Nothing in figmog fetches images
on its own, because Figma's images endpoints are its most rate-limited. See
`docs/SPEC.md` §15.

`figmog --db <store> images <node-id>...` exports matching cached renders
and fills offline. The store does not record its Figma file key, so cache
misses produce per-item errors. To fetch missing images, run
`figmog pull <file-key-or-url>`, then `figmog images <node-id>...` without
`--db`. A node URL in the images arguments selects a node within the chosen
store; it does not supply the store's file identity.

### Freshness is polling, not push

`figmog serve` polls the cheap Tier-3 meta endpoint every `--interval`
seconds (default 10) and spends a Tier-1 file fetch only when the file's
watermark moves. A change shows up within one interval. Figma's `FILE_UPDATE`
webhook is Enterprise-only and debounced up to 30 minutes, so polling a cheap
endpoint beats waiting for it.

### Variables on any plan

On Enterprise, every pull also fetches `variables/local` and folds those
records in. Elsewhere figmog inverts the `boundVariables` references Figma
bakes into the file JSON, which covers each variable's default-mode value.
`figmog import-variables <export.json>` takes a full-fidelity export from
Figma's plugin console on any plan. Each import replaces the previous
snapshot, so tokens deleted in Figma disappear on re-import; an invalid
file is rejected and the last good snapshot kept. `figmog vars --snapshot`
and `figmog status` report the snapshot's state and provenance. See
`docs/SPEC.md` §9.

`figmog tokens export [--out tokens.css]` turns the imported snapshot into
one CSS file: custom properties for the default modes, one collection's
light/dark modes as `:root` and `[data-theme="dark"]`, and a Tailwind v4
`@theme` (`@theme inline` for aliases), so `bg-brand` or `p-card` follow the
theme. Names come from each variable's WEB code syntax, else its kebab-cased
path; a name collision is an error. See `docs/SPEC.md` §9 "Token export".

### A spec of one component

`figmog spec <node>` (tool `figmog_spec`) describes an auto-layout card the
way you rebuild it: direction, gap, padding, alignment and hug/fill/fixed
sizing for every frame, and fill, stroke, radius and typography for every
layer. Each value is shown as the variable token it is bound to, with the
CSS name `figmog tokens export` gives it and the raw value beside it. An
unbound value is flagged as untokenized, and named "matches X" when it
equals a token's light/default value. Grid layout, absolutely positioned
children, mixed text runs, gradients and the like are listed under
`unsupported` instead of being guessed. See `docs/SPEC.md` §17.

### The tools

`figmog serve` exposes 22 `figmog_*` tools: 18 reads of the local mirror,
`figmog_sync`, `figmog_images`, and the two mirror-management tools. Unless
`--no-upstream`, it also proxies Figma's desktop Dev Mode MCP server (paid
Dev or Full seat, desktop app running), merging those tools into the same
list. Desktop calls forward uncached because the active file and revision
cannot be verified against a local mirror. Every name and argument shape is in `docs/SPEC.md` §6 and §8. `figmog tools` prints the
merged list, and `figmog call <tool>` invokes any of them from a shell.

## The benchmark run

<details>
<summary>Per-tool numbers, sync phase, and provenance</summary>

Measured 2026-08-17 on Apple Silicon against a real production file: 5,339
nodes, 5,921,203 bytes, fetched in 4,662.2ms.

Cold sync: 146.0ms flatten plus 81.3ms sync for 5,394 records, 66,364
records a second. Re-pull of the unchanged file: 4.6ms, churn zero.

| tool | calls | p50 (ms) | p95 | p99 | max |
|---|---|---|---|---|---|
| `figmog_node` | 834 | 0.055 | 0.069 | 0.082 | 1.482 |
| `figmog_search` | 834 | 0.067 | 0.124 | 0.191 | 0.621 |
| `figmog_instances` | 833 | 0.111 | 0.125 | 0.151 | 0.607 |
| `figmog_tree` | 833 | 0.462 | 0.529 | 0.733 | 2.172 |
| `figmog_stats` | 833 | 16.892 | 18.587 | 22.385 | 38.561 |
| `figmog_where` | 833 | 22.057 | 23.686 | 27.551 | 40.437 |

Load phase: 5,000 queries in 33.6s, 149 requests a second. API comparison: 5
live calls against the same file, p50 972.6ms, max 1,189.1ms. `figmog_node`
at 0.055ms against the `/nodes` p50 of 972.6ms is the 17,657x figure. At
Figma's Tier-1 budget of ~10 requests a minute, those 5,000 calls take about
500 minutes.

The harness itself was retired from the binary in v0.0.1's slim-down and
lives in the repo history: `docs/history/2026-08-16-figmog-bench.md`.

**Cold CLI reads** (NWS-2941). Every CLI command is a fresh process that
opens the store. After a large pull figmog compacts the store (moves its
writes out of fjall's journal), so that open no longer replays the whole
journal. Synthetic 100,001-node file, best of 3 cold processes, 4-core
Linux, `cargo test --test compact -- --ignored --nocapture`:

| command | before compaction | after |
|---|---|---|
| `figmog path 0:1` | 1.328s | 0.004s |
| `figmog get 500:3` | 1.327s | 0.004s |
| `figmog status` | 1.347s | 0.084s |
| `figmog search "copy number 7"` | 1.334s | 0.076s |
| `figmog vars` | 1.478s | 0.220s |

The journal goes from 93.2 MB to nothing; outputs are byte-identical.
`FIGMOG_NO_COMPACT=1` turns compaction off (`docs/SPEC.md` §4).

</details>

## Install

Download `figmog-<version>-<target>.tar.gz` and `SHA256SUMS` from
[Releases](https://github.com/net-work-studio/figmog/releases). Targets:
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`.

```console
$ shasum -a 256 -c SHA256SUMS --ignore-missing   # sha256sum on Linux
$ tar xzf figmog-<version>-<target>.tar.gz
$ chmod +x figmog
$ xattr -d com.apple.quarantine ./figmog         # macOS: builds are unsigned
$ ./figmog --help
```

The checksum line should print `figmog-<version>-<target>.tar.gz: OK`. The
macOS builds are not signed or notarized, so Gatekeeper refuses to run them
until the quarantine attribute is cleared once. The Linux build is compiled
on Ubuntu 24.04 and needs glibc 2.39 or newer; on an older distribution,
build from source:

```console
$ git clone https://github.com/net-work-studio/figmog.git
$ cd figmog && cargo build --release
```

That needs a Rust toolchain and network access, since the build fetches
`fold` from its pinned git rev.

### Building release tarballs

CI and release jobs run on Blacksmith, using Ubuntu 24.04 for Linux and
macOS 15 on Apple Silicon for both macOS targets.

CI runs tests, Clippy, formatting, and documentation checks on Ubuntu for
pushes to `main` and pull requests. The same checks run on macOS only
on demand via Actions → CI → Run workflow;
select a branch to check macOS compatibility before merging when needed.
From the GitHub CLI, run:

```sh
gh workflow run ci.yml --repo net-work-studio/figmog --ref main
```

New commits cancel older CI runs for the same event and ref. CI does not
build release tarballs. The separate [Release workflow](https://github.com/net-work-studio/figmog/actions/workflows/release.yml)
starts when a `v*` tag is pushed, or when a maintainer runs it manually.
Merging a PR, bumping `Cargo.toml`, or having an existing tag does not start
a release by itself.

To build an existing tag whose release never ran, open Actions → Release →
Run workflow, select `main`, and enter the tag (for example, `v0.0.3`), or
use the GitHub CLI:

```sh
gh workflow run release.yml --repo net-work-studio/figmog --ref main -f tag=v0.0.3
```

This uses the workflow from `main` to build the existing tag's commit; it
does not create or move the tag. To publish a future version, first merge
the version bump and changes into `main`, then tag that commit and push the
new tag (replace `vX.Y.Z` with the version):

```sh
git fetch origin main
git tag -a vX.Y.Z origin/main -m "Release vX.Y.Z"
git push origin vX.Y.Z
```

The Release workflow builds macOS ARM64, macOS Intel, and Linux x86_64
binaries, packages them as three `.tar.gz` files, generates `SHA256SUMS`,
and publishes all four files as a GitHub pre-release. Follow progress in
Actions → Release; downloads appear on the releases page after the publish
job succeeds. No custom repository secrets are required.

## Limitations

- Full per-mode variable values need either an Enterprise plan or a manual
  `import-variables`; inference alone covers the default mode.
- The proxied desktop tools answer for whatever file is open in the Figma
  app. They have no concept of "which file", so a `file` argument on such a
  call is ignored.
- A file change is visible within one poll interval, not instantly.
- `figmog images` spends Figma's most rate-limited endpoints and is never
  called automatically. Cached bytes are keyed to the file version and are
  dropped when the version moves.
- Style definitions are derived from one consumer node, since the file JSON
  carries style metadata only. A style with no consumers has no derivable
  value.
- Instance overrides that Figma does not serialize into the instance's
  subtree are not reconstructed.
- `pull --fresh` wipes the whole store, including imported variables and the
  stored geometry flag.
- macOS and Linux only. The control plane is a unix domain socket and the
  code uses unix filesystem APIs directly, so there is no Windows build.
- One process may open a store at a time (fjall is single-writer).
  The background figmog holds that handle for its life. Other commands, writers
  included, reach it over the socket instead; `--no-socket`, or a `--db`
  naming the store `serve` owns, gets a `store is locked by figmog serve (pid
  …, socket …)` error naming the owner.

## License

figmog is licensed under AGPL-3.0-only. The full text is in
[`LICENSE`](LICENSE). The v0.0.1 release stays MIT, for that snapshot only.
If you want figmog under other terms, email hello@sanctuary.computer and we
will talk.

figmog depends on [`fold`](https://github.com/flowercomputers/bogkit) through
a pinned git dependency, and upstream `bogkit` currently ships with no
license file. Building and running figmog from source is fine. Publishing
binaries that embed `fold` more widely than this repo's own testing releases
waits on upstream adding a license, which is a separate decision from
figmog's own license.

## Figma's terms

figmog works the way Figma's [own API guidance](https://developers.figma.com/docs/rest-api/rate-limits)
asks integrations to work: cache file data instead of refetching it, and back
off when the API says to. Every request it makes uses your token, against
official endpoints, at your seat's normal limits; the mirror exists so there
are far fewer of those requests.

## A note for Figma

You already ship a local MCP server for design agents. Shouldn't it work like
this, answering from the machine the file is already on, with no rate limit
between an agent and a design it is allowed to read?

If you would like to use this code, we would love to give it to you. Email us
at hello@sanctuary.computer.
