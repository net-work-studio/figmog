# @figmog/react (screenshot runner)

The start of the future `@figmog/react` package. For now it holds one small
tool, `figmog-shot`, which produces the visual evidence for a verification
run:

- **capture** one element of a page (URL + CSS selector + viewport) as a PNG
  in headless Chromium, under a pinned, stable rendering environment;
- **compare** a Figma render with "before" and "after" captures, producing
  difference images, one composite image and an advisory score;
- write a **JSON manifest** for both, with sorted keys and a SHA-256 digest
  of every image.

It is private and not published. It does not touch the Rust crate at the
repository root.

## Setup

Requires Node 22.

```sh
cd packages/react
npm ci
npx playwright install chromium   # once per machine: downloads the pinned build
```

`playwright` is pinned to exactly `1.63.0`, so `npx playwright install`
fetches the matching Chromium build (1243). If the browsers live elsewhere,
set `PLAYWRIGHT_BROWSERS_PATH` (for example `/opt/pw-browsers`); to launch a
specific binary instead, set `FIGMOG_SHOT_CHROMIUM` to its path (or pass
`executablePath` to the API).

## Commands

| Command             | What it does                                                         |
| ------------------- | -------------------------------------------------------------------- |
| `npm run build`     | Compile `src/` and `test/` with `tsc` into `dist/`.                  |
| `npm run typecheck` | `tsc --noEmit` with strict settings (unused code, unchecked indexes). |
| `npm run lint`      | `scripts/lint.mjs`: source hygiene rules that `tsc` does not cover.  |
| `npm test`          | Build, then run `node --test` over `dist/test/**/*.test.js`.         |

The tests start a tiny `node:http` server over `test/fixtures/` (a synthetic
card page plus a "wrong padding" variant) and drive real Chromium captures at
1440 px and 390 px.

`test/tokens.test.ts` checks `figmog tokens export` (NWS-2967, NWS-2948) in a real
browser: it compiles the Rust golden file `tests/fixtures/tokens/export.css`
(repository root; kept current by `cargo test --test tokens`, so no cargo
build is needed here) together with `@import "tailwindcss"` using Tailwind's
own `compile` API, loads the result in Chromium and asserts the computed
colours, padding and radius of `bg-brand`, `p-card`, `rounded-md`, ... in
light mode, under `data-theme="dark"` on the root, and in a dark subtree.
It does the same with `tests/fixtures/tokens/modes.css` (NWS-2948:
independent Brand × Theme × Density collections), checking `bg-button-hover`
(a semantic alias chain through Theme into Brand), `pt-card-gap` and
`pl-space-inset` with the theme, brand and density modes nested either way
or set on one element.
`test/spec.test.ts` checks `figmog spec`'s CSS translation (NWS-2947) the
same way: it renders the Rust golden file `tests/fixtures/spec/edge.spec.json`
(the edge-case fixture: negative spacing, wrap, grid, min/max, constraints,
clipping, text auto-resize and truncation, mixed runs, an instance) as nested
elements — once with each node's `css` inline, once with its Tailwind class
hint compiled against `tests/fixtures/spec/tokens.css` — and asserts at
1440 px and 390 px that every node's box equals the design's recorded
bounding box (also inside a CSS border, and with a unitless token as padding),
and that bound tokens, opacity tokens, kept newlines and text rules resolve.
`tailwindcss` (pinned `4.3.4`, a dev dependency) is the compiler itself and
has no dependencies of its own; `@tailwindcss/cli`/`@tailwindcss/node` would
add native `lightningcss` binaries and a file-watcher stack this test does
not need.

## Usage

```sh
# Capture: writes OUT/NAME.png and OUT/NAME.manifest.json (NAME defaults to "capture").
node dist/src/cli.js capture --url http://localhost:5173/ --selector '[data-testid=card]' \
  --viewport 1440x900 --out shots --name after

# Compare: writes OUT/{figma,before,after,diff-before,diff-after,composite}.png and OUT/manifest.json.
node dist/src/cli.js compare --figma figma.png --before shots/before.png --after shots/after.png --out report
```

Once built, the same CLI is the package's `figmog-shot` bin. The programmatic
API (`dist/src/index.js`) exports `capture`, `captureToDir`, `compareToDir`,
`diffImages`, `composite` and helpers.

A capture fails (exit 1) unless the selector matches **exactly one** element:
zero matches and ambiguous selectors both raise `SelectorMatchError`. Usage
errors (missing or unknown options, unknown command) exit 2.

## Stable environment

Every capture uses, and records under `settings` in its manifest:

- the pinned Chromium build that ships with Playwright `1.63.0` (the manifest
  records the Playwright version and the browser's reported version), headless;
- Chromium flags `--disable-lcd-text`, `--font-render-hinting=none`,
  `--force-color-profile=srgb`;
- device pixel ratio `1` and CSS-pixel screenshots;
- `colorScheme: "light"`, `locale: "en-US"`, `timezoneId: "UTC"`;
- `reducedMotion: "reduce"`, an injected stylesheet that zeroes animation and
  transition durations and pauses animations, plus Playwright's
  `animations: "disabled"`;
- a hidden text caret (`caret-color: transparent` and `caret: "hide"`);
- `await document.fonts.ready` after load and before the screenshot.

Each capture launches a fresh browser, so captures do not share state.
Capturing the same element twice gives byte-identical PNGs (tested).

## Comparison, composite and score

All comparisons are against the Figma render. `diff-after.png` compares
"after" with it and `diff-before.png` compares "before" with it. Both images
are aligned top-left on a canvas of the larger width and the larger height.

- Red: pixels that differ inside the area both images cover (`pixelmatch`,
  threshold `0.1`).
- Yellow: anti-aliasing differences. They are shown but not counted.
- Magenta: pixels outside the common area, where the sizes differ.
- Faded grey: matching pixels.

`composite.png` lays out four panels left to right: **Figma | before | after
| difference** (the after-vs-Figma difference). Each panel is labelled above
with a built-in bitmap font. A panel smaller than the largest is padded in
light grey. The order and colours are also listed under `legend` in the
manifest.

**Score (advisory).** For a reference image and a candidate:

```
W x H      = larger width x larger height of the two images
mismatched = pixelmatch mismatches inside the common area
             + every pixel outside the common area (size mismatch)
score      = round(100 * (1 - mismatched / (W * H)), 2 decimals)
```

100 means pixel-identical; a size difference always lowers the score. The
manifest reports `score.after` and `score.before` (with `mismatched` and
`total`) and the formula text. The score is advisory and is never a pass/fail
gate.

## Manifests

Both manifests are written with object keys sorted recursively, two-space
indented, so equal content gives equal bytes. Image paths are file names
relative to the output directory, never absolute paths.

- `NAME.manifest.json` (capture): `image` (`file`, `width`, `height`,
  `sha256`), `selector`, `settings`, `url`, `viewport`.
- `manifest.json` (compare): `images` (`composite`, `diffAfter`,
  `diffBefore`), `inputs` (`figma`, `before`, `after`, each with its digest
  and, when a `NAME.manifest.json` sits next to the input PNG, the embedded
  capture manifest), `legend`, `score`, `settings`, and `viewport` (shared
  viewport of the inputs' captures, or `null` when unknown or mixed).

## Fixtures

Everything under `test/fixtures/` is synthetic. The tests produce the Figma
render by capturing the correct card page. That capture stands in for a real
Figma export until the bridge exists.

## Dependencies and why

Every dependency is pinned exactly; `package-lock.json` is committed.

Runtime:

- **`playwright` 1.63.0**: drives headless Chromium for captures. Required by
  the issue. It is pinned exactly so the browser build (Chromium 1243) is
  fixed. Its only dependency is `playwright-core`.
- **`pngjs` 7.0.0**: decodes and encodes PNGs in pure JavaScript, with no
  dependencies and no native code. It is needed to read captures and the
  Figma render and to write the diff and composite images. Its encoder writes
  no timestamps, so output bytes are deterministic. The alternative was
  rendering images through a Playwright page canvas, which is slower and more
  convoluted.
- **`pixelmatch` 8.0.0**: small, widely used pixel-level image diff with
  anti-aliasing detection, from the Mapbox ecosystem. It has no native code;
  its only dependency is `pngjs`, which this package already uses. Writing a
  perceptual diff with anti-aliasing detection here would duplicate it badly.

Development:

- **`typescript` 7.0.2**: compiler and type-checker (`tsc`). It also builds
  the tests, so no `tsx` or `ts-node` is needed.
- **`@types/node` 22.20.5**: Node 22 API types for `tsc`.
- **`@types/pngjs` 6.0.5**: types for `pngjs`, which ships none.
  (`pixelmatch` ships its own types.)

Deliberately not added:

- **ESLint and typescript-eslint.** Their dependency tree is large. Type-level
  lint rules (unused locals and parameters, implicit `any`, unchecked index
  access, fall-through) are enforced by `tsc` in strict mode. The
  dependency-free `scripts/lint.mjs` covers the remaining rules: no tabs,
  trailing whitespace, explicit `any`, `console` in `src/`, focused or
  skipped tests, `debugger`, `@ts-ignore`, or extensionless relative imports;
  plus line length, final newline and LF line endings.
- **A test framework.** The tests use Node's built-in `node:test`.
