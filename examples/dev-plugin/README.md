# Sample app: the `@figmog/react` dev-server plugin

NWS-2957. A small Vite + React + Tailwind v4 app wired to the
`@figmog/react` Vite plugin: automatic `data-figmog` attributes in
development, the local bridge to the project's figmog, and one verification
target captured through its explicit locator. Everything is synthetic and
offline: the "Figma file" is the repository's
`tests/fixtures/map/pricing.json` (a pricing card component set, a badge
and a star icon), mirrored with `figmog pull --from-file`.

It is a separate example rather than an extension of `examples/slice/app`:
the slice's committed evidence must stay byte-identical across runs and its
builds use no figmog plugin, while this app needs its own `figmog.toml`,
`figmog.map.json` and `figmog.targets.json` (a project root of its own).

## Files

| Path | What |
| --- | --- |
| `figmog.toml` | `[files] default` (the synthetic file's key) and `[inspector] attributes = true`. |
| `figmog.map.json` | PricingCard (component set), Badge and StarIcon (components). |
| `figmog.targets.json` | One target: the Pro card through `data-testid="pricing-pro"`. |
| `src/components/PricingCard.jsx` | Root `<article>`: marked. Rendered twice (Pro and Free). |
| `src/components/StarIcon.jsx` | Root `<svg>`: marked. |
| `src/components/Badge.jsx` | Fragment root: skipped, with a warning. |
| `vite.config.js` | `figmog()` then `tailwindcss()`; exports `config(options)` for the test. |
| `test/e2e.test.mjs` | The end-to-end test. |

## Run it

From a clean checkout (Rust toolchain, Node 22, the pinned Chromium for
Playwright 1.63.0):

```sh
cargo build                                               # target/debug/figmog
(cd packages/react && npm ci && npm run build)            # the plugin
cd examples/dev-plugin && npm ci
PLAYWRIGHT_BROWSERS_PATH=/opt/pw-browsers npm test        # FIGMOG_BIN overrides the binary
```

The test copies the app into a fresh `git init` directory (so its mirror
root and background figmog are its own, never this checkout's), mirrors the
fixture, and then:

- starts a Vite dev server with the plugin and checks in Chromium that both
  cards and both star icons carry `data-figmog` / `data-figmog-node` (`10:1`,
  `13:1`) while the Badge fragment does not, and that the skip is reported;
- calls `figmog_node` and `figmog_spec` from the page through the bridge,
  answered by a real `figmog serve` relay and the project's background owner
  (the spec names `<PricingCard … />` from the map), and checks refusals:
  another origin, `figmog_sync`, a desktop tool, another project's file, a
  wrong token, a LAN `Host`;
- captures the `pricing-pro` target: one PNG of one card per contract
  viewport — while the automatic attribute, shared by both cards, is
  ambiguous;
- runs `vite build` and checks that no built file mentions figmog or the
  session token;
- checks the owner exits once the dev server closes.

To try it by hand, mirror the fixture once and start the dev server:

```sh
figmog pull --from-file ../../tests/fixtures/map/pricing.json
npm run dev
```

Inside this repository the mirror goes to the checkout's shared mirror root
(`<git-common-dir>/figmog/DevPluginFile001/`). `vite.config.js` passes
`serveArgs: ["--no-watch"]` because the file is an offline fixture; a real
project leaves it out, so figmog keeps the mirror current.
