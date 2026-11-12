# Vertical slice: one token snapshot → one component → before/after evidence

NWS-2965. One documented command takes a synthetic Figma file to a committed
token file, a built card and visual evidence at two viewports, offline.

> **Everything here is synthetic, and the Figma render is a stand-in.** No
> Figma access is used. The "Figma file" is a hand-written fixture
> (`figma/card-file.json`, version `4243`) mirrored with `figmog pull
> --from-file`. The "Figma render" (`figma/card-render-1440.png`,
> `figma/card-render-390.png`) is a capture of `figma/standin/index.html`,
> which re-draws the file's card from its raw values in plain CSS. It is
> rendered by the same Chromium and fonts as the app, so it is **not** an
> independent rasteriser, and a 100 "after" score is expected by
> construction. It shows the pipeline works, not fidelity to a real Figma
> raster. Replace those two PNGs with real exports of the same file version
> once the Figma bridge exists.

## Command sequence

From a clean checkout (Rust toolchain, Node 22, the pinned Chromium for
Playwright 1.63.0):

```sh
PLAYWRIGHT_BROWSERS_PATH=/opt/pw-browsers examples/slice/run.sh --test
```

`run.sh` does, in order:

1. `cargo build`: the repository's own `target/debug/figmog`.
2. `npm ci && npm run build` in `packages/react` (`figmog-shot`), then
   `npm ci` in `examples/slice/app`.
3. `node scripts/slice.mjs run` in the app, which:
   1. **Tokens.** In a temporary store (`--db <tmp> --no-socket`: no mirror
      root, no serve relay, no background owner):
      `figmog pull --from-file figma/card-file.json`,
      `figmog import-variables tests/fixtures/spec/variables.json`,
      `figmog tokens export --out app/src/tokens.css`.
   2. **Spec.** `figmog spec 1:1 > spec/card.spec.json`.
   3. Checks that the stand-in render is pinned to the mirrored version
      (`figma/card-render.json` `file.version` = `figmog status` version),
      and that the synthetic file, stand-in HTML and both PNGs match their
      recorded SHA-256 hashes. Editing an input without changing the file
      version still requires re-rendering the stand-in.
   4. **Builds.** `vite build` of three app trees: **before** (a temporary
      copy of the app with `baseline/Card.jsx` swapped in for
      `src/Card.jsx`), **after** (the app as committed) and **broken**
      (`variants/Card.wrong-padding.jsx` swapped in). Each is served from a
      loopback port.
   5. **Captures.** `figmog-shot` captures `[data-testid="slice-card"]` in
      each build at 1440×900 and 390×844 (light mode, default state).
   6. **Evidence.** `figmog-shot compare` against the stand-in render writes
      `evidence/{correct,broken}/{1440,390}/` and `evidence/evidence.json`
      plus `evidence/COMMENT.md`.
4. With `--test`: `npm test` in the app.

The temporary store, app trees and captures are deleted afterwards. The run
is deterministic within the same OS, installed fonts and browser environment:
running it twice there gives byte-identical outputs. A different environment
can change text rasterisation; regenerate the stand-in and evidence together
when moving between environments.

`node scripts/slice.mjs standin` re-renders the stand-in PNGs and
`figma/card-render.json`; `run.sh` never does.
Run it once to upgrade older stand-in records that lack the file and HTML
hashes; the evidence command refuses such records.

## Layout

| Path | What |
| --- | --- |
| `figma/card-file.json` | Synthetic Figma file (GET /v1/files response shape). Its card is the `tests/fixtures/spec/card.json` card reduced to the parts a spec fully describes: Title, and a Meta row with Body and Badge (no grid gallery, gradient ribbon or mixed-run caption), same node ids and variable bindings; body text shortened to fit one line. |
| `tests/fixtures/spec/variables.json` (repository root) | The token snapshot. One source of truth, shared with the `figmog spec` tests; the card file binds to its variable ids. |
| `figma/standin/index.html` | The stand-in design, from the card's raw Figma values. |
| `figma/card-render-{1440,390}.png`, `.manifest.json` | Stand-in "Figma renders" (figmog-shot captures). |
| `figma/card-render.json` | Stand-in record: Figma file version it stands for, image digests, caveat. |
| `spec/card.spec.json` | `figmog spec 1:1` output (generated, committed). |
| `app/src/tokens.css` | `figmog tokens export` output (generated, committed). |
| `app/src/Card.jsx` | The card, built from the spec using only the exported tokens. |
| `app/baseline/Card.jsx` | The unstyled placeholder the card started as: the "before" state. |
| `app/variants/Card.wrong-padding.jsx` | Test fixture: the card with the wrong padding. |
| `app/variants/Card.hardcoded.jsx` | Test fixture: the card with every token value written as a literal. |
| `app/scripts/` | `slice.mjs` (the run) and `lib.mjs` (shared plumbing). |
| `app/test/slice.test.mjs` | The slice tests. |
| `app/test/icons.test.mjs` | NWS-2952: renders the icon components `figmog assets export` generates (`tests/fixtures/assets/export/`) with React and checks that repeated instances never share an id. Needs only `npm ci`. |
| `evidence/` | Run outputs (committed). |

## The card and the tokens

`app/src/index.css` imports Tailwind v4's preflight and utilities but **not
its default theme**, then `tokens.css`. The only theme keys are the exported
tokens, so `bg-surface`, `rounded-md`, `py-4`, `gap-4`, `gap-2`, `bg-brand`
and `text-text-primary` resolve to them, and a utility naming a value the
design system lacks generates nothing. Tokens without a theme entry (font
sizes, line height and stroke width, exported in px, and the unitless font
weight) are used through their CSS variables, e.g.
`text-[length:var(--font-size-title)]`.

Tailwind's automatic source discovery is disabled. It scans only `src/` and
`index.html`, which the build identity hashes and the temporary app copies.
Files outside that scope and previous `dist/` output cannot add utilities.

Values the spec flags `untokenized` (horizontal padding 24, width 320, title
line height 28, body fill and weight, badge size and radius, letter spacing)
have no token, so they are written as arbitrary values and marked
"untokenized" in the source. The font family token is a STRING variable,
which `tokens export` skips, so `Inter, sans-serif` is written directly.

## Evidence contract

- Viewports 1440 px and 390 px; light mode; default state; local only.
- One explicit selector: `[data-testid="slice-card"]`.
- **before** is the baseline placeholder card (`app/baseline/Card.jsx`), the
  state before the card was edited, built into a temporary copy of the app;
  **after** is the committed app.
- `evidence.json` (sorted keys) records, for each build, the git commit, a
  `dirty` flag (slice inputs differ from that commit) and a tree digest
  (SHA-256 over sorted `sha256  path` lines of `index.html`, package files,
  `vite.config.js` and `src/**`); the Figma file version and the stand-in's
  version; the token snapshot's file SHA-256 and figmog digest; the token
  CSS and spec digests; and per run and viewport the scores and the SHA-256
  of every image.
- Build identity is sampled after the token CSS and spec are generated, so
  changed generated inputs are included in the `dirty` flag.
- Score: advisory, `100 * (1 - mismatched / (W*H))` against the Figma
  render (see `packages/react/README.md`); never a pass/fail gate.
- Captured URLs are recorded as `http://127.0.0.1:<port>/`: the port is
  random per run and left out so records are stable.
- `evidence/COMMENT.md` is the ready-to-post PR comment. By default its
  image links are relative to `evidence/` itself (`correct/1440/composite.png`),
  so they render when the file is viewed in the repository. To post it
  elsewhere (a PR comment), regenerate with absolute links:
  `node scripts/slice.mjs run --link-base https://github.com/<owner>/<repo>/blob/<sha>`
  (or `npm run evidence -- --link-base <url>`) in `app/`, which links each
  image as `<url>/<repository path>?raw=true`. Commit only the default form.

## Tests

`cd examples/slice/app && npm test` (needs `cargo build`, packages/react
built, `npm ci` here, and the pinned Chromium; set `FIGMOG_BIN` to use
another figmog binary):

- **Committed outputs are current:** `src/tokens.css` and
  `spec/card.spec.json` equal a fresh export and spec.
- **Evidence shape:** sorted-key JSON, both viewports for the correct and
  the broken card, every image digest matches its file, the Figma render is
  pinned to the file version, snapshot digest recorded.
- **Broken card:** the wrong-padding card shows a non-zero difference and
  scores lower than the correct card at both viewports (live captures, and
  in the committed evidence).
- **Token perturbation:** a temp copy of the snapshot changes `space/4` (16
  → 20) and `ocean/500` (`#0066cc` → `#cc3300`); figmog re-exports it and the
  app is rebuilt with the new `tokens.css` and the card code untouched.
  `getComputedStyle` must show padding-top and gap 20px and the badge
  `rgb(204, 51, 0)`. The same check run on `variants/Card.hardcoded.jsx`
  (identical to the correct card under the committed tokens, also asserted)
  must **fail**, and the test asserts that it does.

## Dependencies and why

All pinned exactly; `package-lock.json` is committed.

- **`react` / `react-dom` 19.3.1**: the sample app is a React app; the issue
  asks for React.
- **`vite` 8.3.5** (dev): builds the app. Vite transforms JSX itself, so
  **`@vitejs/plugin-react` is not used** (it only adds Fast Refresh, which a
  production build does not need).
- **`tailwindcss` 4.3.4** (dev): Tailwind v4 itself, the same pin as
  `packages/react`.
- **`@tailwindcss/vite` 4.3.4** (dev): Tailwind's official Vite integration,
  so utilities are generated from scanned sources the way a real app does
  it. It brings `@tailwindcss/node` and `@tailwindcss/oxide` (native
  scanner); `packages/react` avoided those only because its test compiles a
  fixed candidate list.
- **`playwright` 1.63.0** (dev): `getComputedStyle` assertions in the tests;
  the same pin (and Chromium build) as `packages/react`.

`figmog-shot` is used from `packages/react/dist` by path, not as a package
dependency.
