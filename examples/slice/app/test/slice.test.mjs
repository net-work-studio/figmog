// Tests for the vertical slice (NWS-2965). Need: `cargo build` at the
// repository root (or FIGMOG_BIN), `npm ci && npm run build` in
// packages/react, `npm ci` here, and the pinned Chromium
// (PLAYWRIGHT_BROWSERS_PATH). Offline: no Figma access, no mirror root.
import assert, { AssertionError } from "node:assert/strict";
import { access, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { chromium } from "playwright";
import {
  APP,
  captureCard,
  CARDS,
  EVIDENCE_DIR,
  evidenceLink,
  FIGMA_FILE,
  mirror,
  REPO,
  SELECTOR,
  sha256,
  shot,
  SNAPSHOT,
  SPEC_JSON,
  standinPng,
  TOKENS_CSS,
  variantTree,
  VIEWPORTS,
  withBuild,
} from "../scripts/lib.mjs";

const scratch = await mkdtemp(join(tmpdir(), "figmog-slice-test-"));
const trees = [];
let browser;

before(async () => {
  const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});

after(async () => {
  await browser?.close();
  for (const tree of trees) await rm(tree, { recursive: true, force: true });
  await rm(scratch, { recursive: true, force: true });
});

/** A temporary app tree with `card` (and optionally token CSS) swapped in. */
async function tree(options) {
  const root = await variantTree(options);
  trees.push(root);
  return root;
}

/** Computed styles of the card, its title row gap and its badge, at 1440 px. */
async function cardStyle(origin) {
  const page = await browser.newPage({ viewport: VIEWPORTS[0] });
  try {
    await page.goto(`${origin}/`);
    return await page.evaluate((selector) => {
      const card = document.querySelector(selector);
      const badge = card.querySelector("span");
      const c = getComputedStyle(card);
      return {
        background: c.backgroundColor,
        badge: getComputedStyle(badge).backgroundColor,
        gap: c.rowGap,
        paddingLeft: c.paddingLeft,
        paddingTop: c.paddingTop,
        radius: c.borderTopLeftRadius,
      };
    }, SELECTOR);
  } finally {
    await page.close();
  }
}

/** Values of the committed snapshot (light mode). */
const COMMITTED = {
  background: "rgb(250, 250, 252)",
  badge: "rgb(0, 102, 204)",
  gap: "16px",
  paddingLeft: "24px",
  paddingTop: "16px",
  radius: "12px",
};

describe("committed slice outputs", () => {
  test("tokens.css and card.spec.json are what figmog produces from the committed fixtures", async () => {
    const m = await mirror();
    try {
      const out = join(scratch, "tokens.css");
      m.exportTokens(out);
      assert.equal(await readFile(TOKENS_CSS, "utf8"), await readFile(out, "utf8"), "src/tokens.css is stale: run ../run.sh");
      assert.equal(await readFile(SPEC_JSON, "utf8"), m.spec(), "spec/card.spec.json is stale: run ../run.sh");
    } finally {
      await m.cleanup();
    }
  });

  test("the card source uses no hex colour literal outside untokenized values", async () => {
    const source = await readFile(join(APP, CARDS.after), "utf8");
    for (const line of source.split("\n").filter((l) => /#[0-9a-f]{3,8}\b/i.test(l))) {
      assert.match(line, /untokenized/, `hardcoded colour on a tokenized value: ${line.trim()}`);
    }
  });
});

describe("evidence record", () => {
  let text;
  let evidence;
  before(async () => {
    text = await readFile(join(EVIDENCE_DIR, "evidence.json"), "utf8");
    evidence = JSON.parse(text);
  });

  test("is sorted-key JSON", async () => {
    const { stableJson } = await shot();
    assert.equal(text, stableJson(evidence));
  });

  test("covers both viewports for the correct and the broken card, with matching image digests", async () => {
    assert.deepEqual(evidence.contract.viewports, VIEWPORTS);
    assert.equal(evidence.contract.colorScheme, "light");
    for (const run of ["correct", "broken"]) {
      assert.deepEqual(Object.keys(evidence.runs[run]).sort(), ["1440", "390"]);
      for (const { width } of VIEWPORTS) {
        const entry = evidence.runs[run][String(width)];
        for (const key of ["after", "before", "composite", "diffAfter", "diffBefore", "figma"]) {
          const image = entry.images[key];
          assert.equal(sha256(await readFile(join(REPO, image.path))), image.sha256, image.path);
        }
        assert.equal(entry.images.figma.sha256, sha256(await readFile(standinPng(width))));
      }
    }
  });

  test("records build identity, the Figma version and the token snapshot digest", async () => {
    const file = JSON.parse(await readFile(FIGMA_FILE, "utf8"));
    assert.equal(evidence.figma.file.version, file.version);
    assert.equal(evidence.figma.render.version, file.version, "the Figma render must be pinned to the mirrored version");
    assert.equal(evidence.tokens.snapshot.sha256, sha256(await readFile(SNAPSHOT)));
    assert.match(evidence.tokens.snapshot.digest, /^fnv1a64:[0-9a-f]{16}$/);
    for (const name of ["before", "after", "broken"]) {
      assert.match(evidence.builds[name].commit, /^[0-9a-f]{40}$/);
      assert.match(evidence.builds[name].tree.sha256, /^[0-9a-f]{64}$/);
    }
    assert.notEqual(evidence.builds.before.tree.sha256, evidence.builds.after.tree.sha256);
  });

  test("COMMENT.md image links resolve from its own directory", async () => {
    const comment = await readFile(join(EVIDENCE_DIR, "COMMENT.md"), "utf8");
    const links = [...comment.matchAll(/!\[[^\]]*\]\(([^)]+)\)/g)].map((m) => m[1]);
    assert.equal(links.length, 12);
    for (const link of links) {
      assert.ok(!link.startsWith("/") && !link.startsWith("examples/"), link);
      await access(join(EVIDENCE_DIR, link));
    }
    assert.ok(links.includes("correct/1440/composite.png"), links.join(", "));
  });

  test("with a link base, image links are absolute raw URLs", () => {
    const image = join(EVIDENCE_DIR, "correct", "390", "figma.png");
    assert.equal(evidenceLink(image), "correct/390/figma.png");
    assert.equal(
      evidenceLink(image, "https://github.com/o/r/blob/abc123/"),
      "https://github.com/o/r/blob/abc123/examples/slice/evidence/correct/390/figma.png?raw=true",
    );
  });

  test("the npm evidence script runs the slice", async () => {
    const pkg = JSON.parse(await readFile(join(APP, "package.json"), "utf8"));
    assert.equal(pkg.scripts.evidence, "node scripts/slice.mjs run");
    await access(join(APP, "scripts", "slice.mjs"));
  });

  test("the broken card scores lower than the correct card at both viewports", () => {
    for (const { width } of VIEWPORTS) {
      const w = String(width);
      assert.ok(evidence.runs.broken[w].score.after < evidence.runs.correct[w].score.after, `at ${w}px`);
    }
  });
});

describe("broken card (wrong padding), live", () => {
  test("shows a visible difference and a lower score than the correct card at both viewports", async () => {
    const { decodePng, diffImages } = await shot();
    const scores = {};
    for (const [name, root] of [
      ["correct", APP],
      ["broken", await tree({ card: CARDS.broken })],
    ]) {
      scores[name] = await withBuild(root, async (origin) => {
        const out = {};
        for (const viewport of VIEWPORTS) {
          const png = await captureCard({ origin, viewport, outDir: join(scratch, name), name: String(viewport.width) });
          const figma = decodePng(await readFile(standinPng(viewport.width)));
          out[viewport.width] = diffImages(figma, decodePng(await readFile(png)));
        }
        return out;
      });
    }
    for (const { width } of VIEWPORTS) {
      assert.ok(scores.broken[width].mismatched > 0, `no visible difference at ${width}px`);
      assert.ok(scores.broken[width].score < scores.correct[width].score, `broken does not score lower at ${width}px`);
    }
  });
});

describe("token perturbation", () => {
  let perturbedCss;
  before(async () => {
    // Change two tokens in a temp copy of the committed snapshot: space/4 16 -> 20, ocean/500 #0066cc -> #cc3300.
    const snapshot = JSON.parse(await readFile(SNAPSHOT, "utf8"));
    const set = (name, value) => {
      const variable = Object.values(snapshot.variables).find((v) => v.name === name);
      assert.ok(variable, `no ${name} in the snapshot`);
      for (const mode of Object.keys(variable.valuesByMode)) variable.valuesByMode[mode] = value;
    };
    set("space/4", 20);
    set("ocean/500", { r: 0.8, g: 0.2, b: 0, a: 1 });
    const path = join(scratch, "variables.perturbed.json");
    await writeFile(path, JSON.stringify(snapshot, null, 2));
    const m = await mirror({ snapshot: path });
    try {
      perturbedCss = join(scratch, "tokens.perturbed.css");
      m.exportTokens(perturbedCss);
    } finally {
      await m.cleanup();
    }
  });

  /** Asserts that `card`, rebuilt against the perturbed export, follows the tokens. */
  async function assertFollowsTokens(card) {
    const style = await withBuild(await tree({ card, tokensCss: perturbedCss }), cardStyle);
    assert.deepEqual(style, { ...COMMITTED, badge: "rgb(204, 51, 0)", gap: "20px", paddingTop: "20px" });
  }

  test("with the committed tokens, the correct and the hardcoded card look the same", async () => {
    assert.deepEqual(await withBuild(APP, cardStyle), COMMITTED);
    assert.deepEqual(await withBuild(await tree({ card: CARDS.hardcoded }), cardStyle), COMMITTED);
  });

  test("re-exporting changed tokens changes the card's computed style without touching its code", async () => {
    await assertFollowsTokens(CARDS.after);
  });

  test("the same check FAILS for a hardcoded card (the test catches it)", async () => {
    await assert.rejects(assertFollowsTokens(CARDS.hardcoded), AssertionError);
  });
});
