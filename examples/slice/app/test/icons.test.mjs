// NWS-2952: the icon components `figmog assets export` generates, rendered
// by React. The components are the golden export in
// `tests/fixtures/assets/export/src/icons/` (kept current by `cargo test
// --test assets`, so no cargo build is needed here), compiled with Vite's
// JSX transform and rendered to markup with react-dom/server — no DOM
// emulation, no browser.
//
// Checks: two instances of the same icon (and of different icons) on one
// page never share a gradient, mask, clip-path or filter id, and every
// `url(#…)` / `href="#…"` reference resolves inside its own <svg>;
// single-color icons follow `currentColor` while the multicolor logo keeps
// its colors; the components are decorative (aria-hidden) unless labelled,
// and props override the defaults.
import assert from "node:assert/strict";
import { mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createElement, Fragment } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { transformWithOxc } from "vite";

const APP = fileURLToPath(new URL("../", import.meta.url));
const REPO = fileURLToPath(new URL("../../../../", import.meta.url));
const ICONS = join(REPO, "tests", "fixtures", "assets", "export", "src", "icons");

/** Component name → component. */
const components = {};
let scratch;

before(async () => {
  // Inside the app, so `react` resolves from its node_modules.
  scratch = await mkdtemp(join(APP, ".icons-test-"));
  for (const file of (await readdir(ICONS)).filter((f) => /^Icon\w+\.jsx$/.test(f))) {
    const source = await readFile(join(ICONS, file), "utf8");
    const { code } = await transformWithOxc(source, file, { lang: "jsx", jsx: { runtime: "automatic" } });
    const out = join(scratch, file.replace(/\.jsx$/, ".mjs"));
    await writeFile(out, code);
    const name = file.replace(/\.jsx$/, "");
    components[name] = (await import(pathToFileURL(out).href))[name];
  }
});

after(async () => {
  if (scratch) await rm(scratch, { recursive: true, force: true });
});

function render(...elements) {
  return renderToStaticMarkup(createElement(Fragment, null, ...elements));
}

/** Each top-level <svg>…</svg> of `markup`. */
function svgs(markup) {
  return markup.match(/<svg[\s\S]*?<\/svg>/g) ?? [];
}

const ids = (markup) => [...markup.matchAll(/\sid="([^"]*)"/g)].map((m) => m[1]);
const refs = (markup) => [
  ...[...markup.matchAll(/url\(#([^)]*)\)/g)].map((m) => m[1]),
  ...[...markup.matchAll(/href="#([^"]*)"/g)].map((m) => m[1]),
];

describe("generated icon components", () => {
  test("the golden export has the four synthetic icons", () => {
    assert.deepEqual(Object.keys(components).sort(), ["IconArrowRight", "IconLogo", "IconStar", "IconUnsafe"]);
  });

  test("repeated instances never share an id, and references stay inside their own svg", () => {
    const { IconLogo, IconStar, IconArrowRight } = components;
    const markup = render(
      createElement(IconLogo),
      createElement(IconLogo),
      createElement(IconStar),
      createElement(IconStar),
      createElement(IconArrowRight),
      createElement(IconLogo, { className: "size-6" }),
    );
    const all = ids(markup);
    // Logo: mask + gradient; star: clip path. Three logos, two stars.
    assert.equal(all.length, 3 * 2 + 2 * 1);
    assert.equal(new Set(all).size, all.length, `duplicate ids: ${all.join(", ")}`);
    for (const svg of svgs(markup)) {
      const own = new Set(ids(svg));
      for (const ref of refs(svg)) assert.ok(own.has(ref), `#${ref} does not resolve inside ${svg}`);
      for (const id of own) assert.match(id, /^[A-Za-z0-9_-]+$/);
    }
  });

  test("independent roots keep distinct identifier prefixes distinct", () => {
    const { IconLogo, IconStar } = components;
    const prefixes = ["a:b", "ab", "a b", "a/b", "😀", "😁"];
    const markup = prefixes.map((identifierPrefix) => renderToStaticMarkup(
      createElement(Fragment, null, createElement(IconLogo), createElement(IconStar)),
      { identifierPrefix },
    )).join("");
    const all = ids(markup);
    assert.equal(all.length, prefixes.length * 3);
    assert.equal(new Set(all).size, all.length, `duplicate ids across roots: ${all.join(", ")}`);
    for (const svg of svgs(markup)) {
      const own = new Set(ids(svg));
      for (const ref of refs(svg)) assert.ok(own.has(ref), `#${ref} does not resolve inside ${svg}`);
      for (const id of own) assert.match(id, /^[A-Za-z0-9_-]+$/);
    }
  });

  test("single-color icons follow currentColor; the multicolor logo keeps its colors", () => {
    const { IconLogo, IconStar, IconArrowRight } = components;
    const star = render(createElement(IconStar));
    assert.match(star, /fill="currentColor"/);
    assert.doesNotMatch(star, /#0F172A/i);
    // The clip rectangle is geometry, not paint: unchanged.
    assert.match(star, /<rect width="24" height="24" fill="white"/);
    assert.match(render(createElement(IconArrowRight)), /stroke="currentColor"/);
    const logo = render(createElement(IconLogo));
    assert.doesNotMatch(logo, /currentColor/);
    assert.match(logo, /stop-color="#7C3AED"/);
    assert.match(logo, /stop-color="#DB2777"/);
  });

  test("decorative by default; labels and props win", () => {
    const { IconStar } = components;
    assert.match(render(createElement(IconStar)), /aria-hidden="true"/);
    const labelled = render(createElement(IconStar, { "aria-label": "Favourite", width: 32, className: "text-red-500" }));
    assert.doesNotMatch(labelled, /aria-hidden/);
    assert.match(labelled, /aria-label="Favourite"/);
    assert.match(labelled, /width="32"/);
    assert.match(labelled, /class="text-red-500"/);
  });
});
