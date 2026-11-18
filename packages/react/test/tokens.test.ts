// NWS-2967: the CSS `figmog tokens export` writes, compiled with Tailwind v4
// and loaded in real Chromium, makes Tailwind utilities resolve to the
// variable values of each mode. The input is the Rust golden file
// `tests/fixtures/tokens/export.css` (kept current by `cargo test --test
// tokens`), so this test needs no cargo build.
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
const require = createRequire(import.meta.url);

/** The utilities the page uses; Tailwind only generates what it is asked for. */
const CANDIDATES = ["bg-brand", "bg-action-primary", "bg-action-hover", "bg-surface", "bg-overlay", "p-card", "p-inset", "rounded-md"];

/** Compile `@import "tailwindcss"` plus the exported tokens with Tailwind's own compiler. */
async function compileTokens(tokens: string): Promise<string> {
  const compiler = await compile(`@import "tailwindcss";\n${tokens}`, {
    base: dirname(GOLDEN),
    loadStylesheet: async (id, base) => {
      const path = id.startsWith(".") ? join(base, id) : require.resolve(id === "tailwindcss" ? "tailwindcss/index.css" : id);
      return { path, base: dirname(path), content: await readFile(path, "utf8") };
    },
  });
  return compiler.build(CANDIDATES);
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
let css = "";

before(async () => {
  css = await compileTokens(await readFile(GOLDEN, "utf8"));
  const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
  browser = await chromium.launch({
    headless: true,
    ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
  });
  page = await browser.newPage();
  await page.setContent(`<!doctype html><html><head><style>${css}</style></head><body>${BODY}</body></html>`);
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
