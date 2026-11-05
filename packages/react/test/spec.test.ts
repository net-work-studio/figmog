// NWS-2947: the CSS `figmog spec` derives from a design, rendered in real
// Chromium at both contract viewports (1440 px and 390 px), lands every node
// where the design recorded it. The spec itself is viewport-independent JSON;
// what the viewports test is its translation: the edge-case fixture's spec
// (`tests/fixtures/spec/edge.spec.json`, kept current by `cargo test --test
// spec`, so no cargo build is needed here) is turned into nested elements
// twice — once with each node's `css` declarations inline, once with its
// Tailwind class hint compiled by Tailwind v4 against the token export of the
// same variables (`tests/fixtures/spec/tokens.css`) — and each element's box,
// relative to the root, must equal the node's recorded `absoluteBoundingBox`.
//
// The fixture also pins the review findings: a unitless token used as a
// length (scaled to px), opacity tokens in % and unitless, absolute children
// inside a CSS border, and a text with a newline and a run of spaces (two
// lines high only if both are kept).
//
// Skipped, because the spec says so: nodes under an `unsupported` entry about
// position (their placement is deliberately not specified), and the width of
// a text node that hugs its content (it depends on the font, which the
// fixture can't pin).
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
const FIXTURES = join(REPO, "tests", "fixtures", "spec");
const require = createRequire(import.meta.url);

const VIEWPORTS = [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
] as const;

interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface Run {
  text: string;
  css?: Record<string, string>;
  tailwind?: string;
}

interface SpecNode {
  id: string;
  type: string;
  design?: { absoluteBoundingBox?: Box };
  layout?: { sizing?: { horizontal: string; vertical: string } };
  values?: object;
  css?: Record<string, string>;
  tailwind?: string;
  content?: { text?: string; runs?: Run[] };
  children: SpecNode[];
}

interface Spec {
  node: SpecNode;
  unsupported: { id: string; reason: string }[];
}

type Mode = "css" | "tailwind";

function escape(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function inline(css: Record<string, string> | undefined): string {
  return Object.entries(css ?? {})
    .map(([property, value]) => `${property}: ${value}`)
    .join("; ");
}

function attributes(mode: Mode, css: Record<string, string> | undefined, tailwind: string | undefined): string {
  return mode === "css" ? `style="${escape(inline(css))}"` : `class="${escape(tailwind ?? "")}"`;
}

/** The node as an element; a hidden layer (no `values`) renders nothing. */
function render(node: SpecNode, mode: Mode): string {
  if (node.values === undefined) return "";
  let inner = "";
  if (node.content?.runs !== undefined) {
    inner = node.content.runs.map((run) => `<span ${attributes(mode, run.css, run.tailwind)}>${escape(run.text)}</span>`).join("");
  } else if (node.content?.text !== undefined) {
    inner = escape(node.content.text);
  } else {
    inner = node.children.map((child) => render(child, mode)).join("");
  }
  return `<div data-id="${escape(node.id)}" ${attributes(mode, node.css, node.tailwind)}>${inner}</div>`;
}

function walk(node: SpecNode, visit: (node: SpecNode, ancestors: SpecNode[]) => void, ancestors: SpecNode[] = []): void {
  visit(node, ancestors);
  for (const child of node.children) walk(child, visit, [...ancestors, node]);
}

function candidates(spec: Spec): string[] {
  const out = new Set<string>();
  walk(spec.node, (node) => {
    for (const hint of [node.tailwind, ...(node.content?.runs ?? []).map((run) => run.tailwind)]) {
      for (const cls of (hint ?? "").split(" ")) if (cls !== "") out.add(cls);
    }
  });
  return [...out].sort();
}

/** Every rendered element's box, relative to the root element. */
async function boxes(page: Page, rootId: string): Promise<Record<string, Box>> {
  return page.evaluate((root) => {
    const origin = document.querySelector(`[data-id="${root}"]`);
    if (origin === null) throw new Error(`missing root ${root}`);
    const o = origin.getBoundingClientRect();
    const out: Record<string, { x: number; y: number; width: number; height: number }> = {};
    for (const element of document.querySelectorAll("[data-id]")) {
      const r = element.getBoundingClientRect();
      out[element.getAttribute("data-id") ?? ""] = { x: r.x - o.x, y: r.y - o.y, width: r.width, height: r.height };
    }
    return out;
  }, rootId);
}

let browser: Browser | undefined;
let spec: Spec;
const pages = new Map<Mode, Page>();

before(async () => {
  spec = JSON.parse(await readFile(join(FIXTURES, "edge.spec.json"), "utf8")) as Spec;
  const tokens = await readFile(join(FIXTURES, "tokens.css"), "utf8");
  const compiler = await compile(`@import "tailwindcss";\n${tokens}`, {
    base: FIXTURES,
    loadStylesheet: async (id, base) => {
      const path = id.startsWith(".") ? join(base, id) : require.resolve(id === "tailwindcss" ? "tailwindcss/index.css" : id);
      return { path, base: dirname(path), content: await readFile(path, "utf8") };
    },
  });
  const tailwindCss = compiler.build(candidates(spec));
  const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
  browser = await chromium.launch({
    headless: true,
    ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
  });
  const heads: Record<Mode, string> = {
    // The spec's CSS layer on its own, with the token custom properties
    // (`@theme` blocks are unknown at-rules to a browser and ignored).
    css: `<style>body { margin: 0; }</style><style>${tokens}</style>`,
    tailwind: `<style>${tailwindCss}</style>`,
  };
  for (const mode of ["css", "tailwind"] as const) {
    const page = await browser.newPage();
    await page.setContent(`<!doctype html><html><head>${heads[mode]}</head><body>${render(spec.node, mode)}</body></html>`);
    pages.set(mode, page);
  }
});

after(async () => {
  await browser?.close();
});

function page(mode: Mode): Page {
  const found = pages.get(mode);
  if (found === undefined) throw new Error(`no ${mode} page`);
  return found;
}

describe("figmog spec's CSS translation in Chromium", () => {
  for (const mode of ["css", "tailwind"] as const) {
    for (const viewport of VIEWPORTS) {
      test(`${mode}: every node lands on its recorded box at ${viewport.width}px`, async () => {
        await page(mode).setViewportSize(viewport);
        const rendered = await boxes(page(mode), spec.node.id);
        const unplaced = new Set(spec.unsupported.filter((u) => u.reason.includes("position")).map((u) => u.id));
        const origin = spec.node.design?.absoluteBoundingBox;
        assert.ok(origin !== undefined, "the root has no recorded box");
        let checked = 0;
        walk(spec.node, (node, ancestors) => {
          const recorded = node.design?.absoluteBoundingBox;
          if (recorded === undefined || node.values === undefined) return;
          if ([node, ...ancestors].some((n) => unplaced.has(n.id))) return;
          const box = rendered[node.id];
          assert.ok(box !== undefined, `${node.id} not rendered`);
          const hugText = node.type === "TEXT" && node.layout?.sizing?.horizontal === "hug";
          const want: Box = { x: recorded.x - origin.x, y: recorded.y - origin.y, width: recorded.width, height: recorded.height };
          for (const key of ["x", "y", "width", "height"] as const) {
            if (key === "width" && hugText) continue;
            assert.ok(Math.abs(box[key] - want[key]) < 0.5, `${node.id} ${key}: rendered ${box[key]}, recorded ${want[key]}`);
          }
          checked += 1;
        });
        // Every node of the fixture but the rotated one (and nothing under it).
        assert.equal(checked, 43);
      });
    }

    test(`${mode}: bound tokens and text rules resolve`, async () => {
      const computed = await page(mode).evaluate(() => {
        const style = (id: string): CSSStyleDeclaration => {
          const element = document.querySelector(`[data-id="${id}"]`);
          if (element === null) throw new Error(`missing ${id}`);
          return getComputedStyle(element);
        };
        const run = document.querySelector('[data-id="2:33"] span:nth-child(2)');
        if (run === null) throw new Error("missing run");
        return {
          surface: style("2:1").backgroundColor,
          radius: style("2:1").borderTopLeftRadius,
          brand: style("2:40").backgroundColor,
          gap: style("2:1").rowGap,
          clip: style("2:20").overflow,
          truncated: [style("2:34").textOverflow, style("2:34").whiteSpace],
          runWeight: getComputedStyle(run).fontWeight,
          runColor: getComputedStyle(run).color,
          tags: style("2:7").outlineColor,
          // A unitless token, scaled to px; opacity tokens in % and unitless.
          buttonPadding: style("2:40").paddingLeft,
          opacityPercent: style("2:17").opacity,
          opacityUnitless: style("2:50").opacity,
          // Newlines and runs of spaces are kept.
          lines: (document.querySelector('[data-id="2:36"]') as HTMLElement | null)?.innerText ?? "",
        };
      });
      assert.deepEqual(computed, {
        surface: "rgb(250, 250, 252)",
        radius: "12px",
        brand: "rgb(0, 102, 204)",
        gap: "16px",
        clip: "hidden",
        truncated: ["ellipsis", "pre"],
        runWeight: "700",
        runColor: "rgb(0, 102, 204)",
        tags: "rgb(224, 227, 230)",
        buttonPadding: "12px",
        opacityPercent: "0.9",
        opacityUnitless: "0.9",
        lines: "First line\nSecond  line",
      });
    });
  }
});
