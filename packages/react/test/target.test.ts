// Verification targets (NWS-2957): the runner wrapped for figmog's verify
// loop, driven by a target manifest with explicit locators.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { captureTarget, locatorSelector, parseTargets, SelectorMatchError } from "../src/index.js";
import { decodePng } from "../src/png.js";
import { serveFixtures } from "./server.js";

const CLI = fileURLToPath(new URL("../src/cli.js", import.meta.url));

const MANIFEST = {
  version: 1,
  targets: [
    { name: "card", figma: { node: "1:2" }, route: "/card.html", locator: { testid: "card" } },
    { name: "missing", figma: { node: "1:3" }, route: "/card.html", locator: { testid: "nope" } },
    { name: "tags", figma: { node: "1:4" }, route: "/card.html?x=1", locator: { css: ".card__tag" } },
  ],
};

let origin = "";
let close: () => Promise<void> = async () => {};
let work = "";
let manifest = "";

before(async () => {
  ({ origin, close } = await serveFixtures());
  work = await mkdtemp(join(tmpdir(), "figmog-target-"));
  manifest = join(work, "figmog.targets.json");
  await writeFile(manifest, JSON.stringify(MANIFEST));
});

after(async () => {
  await close();
  await rm(work, { recursive: true, force: true });
});

describe("target manifest", () => {
  test("parses and turns locators into selectors", () => {
    const targets = parseTargets(JSON.stringify(MANIFEST));
    assert.deepEqual(
      targets.map((t) => [t.name, locatorSelector(t.locator)]),
      [
        ["card", '[data-testid="card"]'],
        ["missing", '[data-testid="nope"]'],
        ["tags", ".card__tag"],
      ],
    );
    assert.equal(locatorSelector({ testid: 'a"b\\c\nd' }), '[data-testid="a\\"b\\\\c\\a d"]');
  });

  test("rejects invalid manifests, naming the JSON path", () => {
    const target = MANIFEST.targets[0];
    const cases: [unknown, RegExp][] = [
      [{ version: 2, targets: [] }, /version: expected 1/],
      [{ version: 1 }, /targets: expected an array/],
      [{ version: 1, targets: [], extra: 1 }, /\$\.extra: unknown key/],
      [{ version: 1, targets: [{ ...target, viewport: 1 }] }, /targets\[0\]\.viewport: unknown key/],
      [{ version: 1, targets: [{ ...target, name: "Card" }] }, /targets\[0\]\.name: "Card" is not lowercase/],
      [{ version: 1, targets: [target, target] }, /targets\[1\]\.name: duplicate target "card"/],
      [{ version: 1, targets: [{ ...target, route: "https://evil.example/" }] }, /targets\[0\]\.route: expected a path/],
      [{ version: 1, targets: [{ ...target, route: "//evil.example/" }] }, /targets\[0\]\.route/],
      [{ version: 1, targets: [{ ...target, locator: {} }] }, /locator: expected exactly one/],
      [{ version: 1, targets: [{ ...target, locator: { testid: "a", css: "b" } }] }, /locator: expected exactly one/],
      [{ version: 1, targets: [{ ...target, locator: { xpath: "//a" } }] }, /locator\.xpath: unknown key/],
      [{ version: 1, targets: [{ ...target, figma: {} }] }, /figma\.node: expected a non-empty string/],
    ];
    for (const [value, message] of cases) {
      assert.throws(() => parseTargets(JSON.stringify(value), "t.json"), message, JSON.stringify(value));
    }
    assert.throws(() => parseTargets("{", "t.json"), /t\.json: \$: not JSON/);
  });
});

describe("captureTarget", () => {
  test("returns one PNG of the component per contract viewport", async () => {
    const out = join(work, "card");
    const result = await captureTarget({ baseUrl: origin, manifest, outDir: out, target: "card" });
    assert.equal(result.url, `${origin}/card.html`);
    assert.deepEqual(
      result.captures.map((c) => [c.viewport.width, c.pngPath.slice(out.length + 1)]),
      [
        [1440, "card-1440x900.png"],
        [390, "card-390x844.png"],
      ],
    );
    for (const capture of result.captures) {
      const png = decodePng(await readFile(capture.pngPath));
      assert.equal(png.width, capture.manifest.image.width);
      assert.ok(png.width > 0 && png.width <= capture.viewport.width && png.height > 0);
    }
    const summary = JSON.parse(await readFile(result.summaryPath, "utf8")) as Record<string, unknown>;
    assert.equal(summary["kind"], "figmog-shot/target");
    assert.deepEqual(summary["target"], MANIFEST.targets[0]);
  });

  test("fails on zero and on multiple matches, and on an unknown target", async () => {
    const options = { baseUrl: origin, manifest, outDir: join(work, "x"), viewports: [{ width: 390, height: 844 }] };
    await assert.rejects(
      captureTarget({ ...options, target: "missing" }),
      (e: unknown) => e instanceof SelectorMatchError && e.count === 0,
    );
    await assert.rejects(
      captureTarget({ ...options, target: "tags" }),
      (e: unknown) => e instanceof SelectorMatchError && e.count === 3,
    );
    await assert.rejects(captureTarget({ ...options, target: "nope" }), /no target "nope" \(targets: card, missing, tags\)/);
  });

  test("CLI: figmog-shot target prints the summary path, exits 1 on an ambiguous locator", async () => {
    const run = promisify(execFile);
    const out = join(work, "cli");
    const base = ["--manifest", manifest, "--base-url", origin, "--out", out, "--viewport", "390x844"];
    const { stdout } = await run(process.execPath, [CLI, "target", "--target", "card", ...base]);
    assert.equal(stdout.trim(), join(out, "card.target.json"));
    await assert.rejects(run(process.execPath, [CLI, "target", "--target", "tags", ...base]), (e: unknown) => {
      const error = e as { code?: number; stderr?: string };
      return error.code === 1 && /ambiguous/.test(error.stderr ?? "");
    });
  });
});
