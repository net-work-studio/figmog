// NWS-2967, NWS-2948: the CSS `figmog tokens export` writes, compiled with
// Tailwind v4 and loaded in real Chromium, makes Tailwind utilities resolve
// to the variable values of each mode — also through nested theme aliases
// and with independent collections' modes nested either way. The inputs are
// the Rust golden files `tests/fixtures/tokens/export.css` and `modes.css`
// (kept current by `cargo test --test tokens`), so this test needs no cargo
// build.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { chromium, type Browser, type Page } from "playwright";
import { compile } from "tailwindcss";

/** Repository root, from `dist/test/` in the package. */
const REPO = fileURLToPath(new URL("../../../../", import.meta.url));
const GOLDEN = join(REPO, "tests", "fixtures", "tokens", "export.css");
/** Brand × Theme × Density, `Brand/Globex` mapped to `.brand-globex`. */
const MODES_GOLDEN = join(REPO, "tests", "fixtures", "tokens", "modes.css");
const require = createRequire(import.meta.url);

/** The utilities the page uses; Tailwind only generates what it is asked for. */
const CANDIDATES = ["bg-brand", "bg-action-primary", "bg-action-hover", "bg-surface", "bg-overlay", "p-card", "p-inset", "rounded-md"];

/** Compile `@import "tailwindcss"` plus the exported tokens with Tailwind's own compiler. */
async function compileTokens(tokens: string, candidates: string[] = CANDIDATES): Promise<string> {
  const compiler = await compile(`@import "tailwindcss";\n${tokens}`, {
    base: dirname(GOLDEN),
    loadStylesheet: async (id, base) => {
      const path = id.startsWith(".") ? join(base, id) : require.resolve(id === "tailwindcss" ? "tailwindcss/index.css" : id);
      return { path, base: dirname(path), content: await readFile(path, "utf8") };
    },
  });
  return compiler.build(candidates);
}

const BODY = `
<div id="card" class="bg-brand p-card rounded-md">card</div>
<div id="action" class="bg-action-primary">action</div>
<div id="surface" class="bg-surface">surface</div>
<div id="overlay" class="bg-overlay">overlay</div>
<div id="inset" class="p-inset">inset</div>
<section data-theme="dark">
  <div id="nested" class="bg-action-primary p-card">nested dark</div>
  <div id="nested-raw" style="background-color: var(--color-action-primary)">raw var</div>
  <div id="nested-hover" class="bg-action-hover">fallback-named, two-level alias</div>
</section>
`;

interface Styles {
  card: { background: string; padding: string; radius: string };
  action: string;
  surface: string;
  overlay: string;
  nested: { background: string; padding: string };
  /** `var(--color-action-primary)` read directly in the dark subtree. */
  nestedRaw: string;
  /** `bg-action-hover`: `--action-hover → --color-action-primary → --color-brand`. */
  nestedHover: string;
  /** `p-inset`: a GAP alias of the unitless `--dimension-base: 16`. */
  inset: string;
}

async function styles(page: Page): Promise<Styles> {
  return page.evaluate(() => {
    const style = (id: string): CSSStyleDeclaration => {
      const element = document.getElementById(id);
      if (element === null) throw new Error(`missing #${id}`);
      return getComputedStyle(element);
    };
    const card = style("card");
    const nested = style("nested");
    return {
      card: { background: card.backgroundColor, padding: card.paddingTop, radius: card.borderTopLeftRadius },
      action: style("action").backgroundColor,
      surface: style("surface").backgroundColor,
      overlay: style("overlay").backgroundColor,
      nested: { background: nested.backgroundColor, padding: nested.paddingTop },
      nestedRaw: style("nested-raw").backgroundColor,
      nestedHover: style("nested-hover").backgroundColor,
      inset: style("inset").paddingTop,
    };
  });
}

let browser: Browser | undefined;
let page: Page;
let modesPage: Page;
let css = "";
let modesCss = "";

/** The utilities the nested-modes page uses. */
const MODE_CANDIDATES = ["bg-button-hover", "pt-card-gap", "pl-space-inset"];

/**
 * Every probe has `bg-button-hover` (`--button-hover → --color-button-bg →
 * --color-action → --brand-primary|--brand-soft → a primitive`: a semantic
 * alias chain through Theme into Brand), `pt-card-gap` (`--spacing-card-gap
 * → --space-section`, which aliases Density's `--space-card` in Light and
 * is 32px in Dark) and `pl-space-inset` (Density: a px alias of a unitless
 * FLOAT, or 8px when compact). Modes nest both ways and share elements.
 */
const UTILITIES = MODE_CANDIDATES.join(" ");
const PROBES = ["root", "dark", "globex", "compact", "dark-globex", "globex-dark", "dark-compact", "compact-dark", "all"];
const probe = (id: string, attrs = "", extraClass = ""): string =>
  `<div id="${id}"${attrs} class="${extraClass}${UTILITIES}">${id}</div>`;
const MODES_BODY = [
  probe("root"),
  `<section data-theme="dark">${probe("dark")}</section>`,
  `<section class="brand-globex">${probe("globex")}</section>`,
  `<section data-density="compact">${probe("compact")}</section>`,
  `<section data-theme="dark"><div class="brand-globex">${probe("dark-globex")}</div></section>`,
  `<section class="brand-globex"><div data-theme="dark">${probe("globex-dark")}</div></section>`,
  `<section data-theme="dark"><div data-density="compact">${probe("dark-compact")}</div></section>`,
  `<section data-density="compact"><div data-theme="dark">${probe("compact-dark")}</div></section>`,
  probe("all", ' data-theme="dark" data-density="compact"', "brand-globex "),
].join("\n");

interface Probe {
  background: string;
  gap: string;
  inset: string;
}

async function probes(target: Page): Promise<Record<string, Probe>> {
  return target.evaluate((ids) => {
    const out: Record<string, { background: string; gap: string; inset: string }> = {};
    for (const id of ids) {
      const element = document.getElementById(id);
      if (element === null) throw new Error(`missing #${id}`);
      const style = getComputedStyle(element);
      out[id] = { background: style.backgroundColor, gap: style.paddingTop, inset: style.paddingLeft };
    }
    return out;
  }, PROBES);
}

before(async () => {
  css = await compileTokens(await readFile(GOLDEN, "utf8"));
  modesCss = await compileTokens(await readFile(MODES_GOLDEN, "utf8"), MODE_CANDIDATES);
  const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
  browser = await chromium.launch({
    headless: true,
    ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
  });
  page = await browser.newPage();
  await page.setContent(`<!doctype html><html><head><style>${css}</style></head><body>${BODY}</body></html>`);
  modesPage = await browser.newPage();
  await modesPage.setContent(`<!doctype html><html><head><style>${modesCss}</style></head><body>${MODES_BODY}</body></html>`);
});

after(async () => {
  await browser?.close();
});

describe("tokens export through Tailwind v4", () => {
  test("generates the utilities from the exported theme", () => {
    for (const utility of CANDIDATES) assert.ok(css.includes(`.${utility}`), `no .${utility} rule in the compiled CSS`);
    // Aliases are inlined, so the utility reads the alias target where it is used.
    assert.match(css, /\.bg-action-primary\s*\{\s*background-color:\s*var\(--color-brand\)/);
  });

  test("light mode (:root) resolves to the Light values", async () => {
    assert.deepEqual(await styles(page), {
      card: { background: "rgb(0, 102, 204)", padding: "16px", radius: "8px" },
      action: "rgb(0, 102, 204)",
      surface: "rgb(250, 250, 252)",
      overlay: "rgba(0, 0, 0, 0.5)",
      // A dark subtree follows the dark mode even under a light root —
      // also through aliases declared outside the theme collection.
      nested: { background: "rgb(102, 179, 255)", padding: "24px" },
      nestedRaw: "rgb(102, 179, 255)",
      nestedHover: "rgb(102, 179, 255)",
      // A px alias of a unitless FLOAT keeps its unit.
      inset: "16px",
    });
  });

  test('data-theme="dark" on the root resolves to the Dark values', async () => {
    await page.evaluate(() => document.documentElement.setAttribute("data-theme", "dark"));
    try {
      assert.deepEqual(await styles(page), {
        card: { background: "rgb(102, 179, 255)", padding: "24px", radius: "8px" },
        action: "rgb(102, 179, 255)",
        surface: "rgb(26, 26, 31)",
        overlay: "rgba(255, 255, 255, 0.25)",
        nested: { background: "rgb(102, 179, 255)", padding: "24px" },
        nestedRaw: "rgb(102, 179, 255)",
        nestedHover: "rgb(102, 179, 255)",
        inset: "16px",
      });
    } finally {
      await page.evaluate(() => document.documentElement.removeAttribute("data-theme"));
    }
  });
});

describe("nested modes of independent collections through Tailwind v4", () => {
  const ocean500 = "rgb(0, 102, 204)";
  const ocean300 = "rgb(102, 179, 255)";
  const ember500 = "rgb(204, 51, 0)";
  const ember300 = "rgb(255, 153, 102)";

  test("generates the utilities and keeps semantic aliases inline", () => {
    for (const utility of MODE_CANDIDATES) assert.ok(modesCss.includes(`.${utility}`), `no .${utility} rule in the compiled CSS`);
    assert.match(modesCss, /\.bg-button-hover\s*\{\s*background-color:\s*var\(--button-hover\)/);
    assert.match(modesCss, /\.pt-card-gap\s*\{\s*padding-top:\s*var\(--space-section\)/);
  });

  test("each mode, nested either way or on one element, resolves through the alias chains", async () => {
    assert.deepEqual(await probes(modesPage), {
      root: { background: ocean500, gap: "24px", inset: "4px" },
      dark: { background: ocean300, gap: "32px", inset: "4px" },
      globex: { background: ember500, gap: "24px", inset: "4px" },
      compact: { background: ocean500, gap: "12px", inset: "8px" },
      // Theme's alias into Brand follows both modes whichever is outside.
      "dark-globex": { background: ember300, gap: "32px", inset: "4px" },
      "globex-dark": { background: ember300, gap: "32px", inset: "4px" },
      // Theme's Dark value (32px) wins over Density's alias either way.
      "dark-compact": { background: ocean300, gap: "32px", inset: "8px" },
      "compact-dark": { background: ocean300, gap: "32px", inset: "8px" },
      all: { background: ember300, gap: "32px", inset: "8px" },
    });
  });

  test("modes set on the root element apply to the whole page", async () => {
    await modesPage.evaluate(() => {
      document.documentElement.classList.add("brand-globex");
      document.documentElement.setAttribute("data-density", "compact");
    });
    try {
      const all = await probes(modesPage);
      assert.deepEqual(all["root"], { background: ember500, gap: "12px", inset: "8px" });
      assert.deepEqual(all["dark"], { background: ember300, gap: "32px", inset: "8px" });
    } finally {
      await modesPage.evaluate(() => {
        document.documentElement.classList.remove("brand-globex");
        document.documentElement.removeAttribute("data-density");
      });
    }
  });
});
