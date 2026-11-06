import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import {
  capture,
  captureToDir,
  compareToDir,
  SelectorMatchError,
  sha256,
  type CompareManifest,
} from "../src/index.js";
import { readPng } from "../src/png.js";
import { serveFixtures } from "./server.js";

const CLI = fileURLToPath(new URL("../src/cli.js", import.meta.url));
const VIEWPORTS = [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
] as const;

let origin = "";
let close: () => Promise<void> = async () => {};
let work = "";

before(async () => {
  ({ origin, close } = await serveFixtures());
  work = await mkdtemp(join(tmpdir(), "figmog-shot-"));
});

after(async () => {
  await close();
  await rm(work, { recursive: true, force: true });
});

describe("capture", () => {
  for (const viewport of VIEWPORTS) {
    test(`capturing the same element twice is byte-identical at ${viewport.width}px`, async () => {
      const options = { url: `${origin}/card.html`, selector: ".card", viewport };
      const first = await capture(options);
      const second = await capture(options);
      assert.ok(first.png.equals(second.png), "PNG bytes differ between two captures");
      assert.equal(first.sha256, second.sha256);
      assert.equal(first.sha256, sha256(first.png));
      assert.ok(first.width > 0 && first.width <= viewport.width);
    });
  }

  test("records the stable-environment settings", async () => {
    const dir = join(work, "settings");
    const { manifest, manifestPath } = await captureToDir({
      url: `${origin}/card.html`,
      selector: ".card",
      viewport: { width: 1440, height: 900 },
      outDir: dir,
      name: "card",
    });
    const onDisk = JSON.parse(await readFile(manifestPath, "utf8")) as typeof manifest;
    assert.deepEqual(onDisk, manifest);
    assert.equal(manifest.settings.deviceScaleFactor, 1);
    assert.equal(manifest.settings.colorScheme, "light");
    assert.equal(manifest.settings.reducedMotion, "reduce");
    assert.equal(manifest.settings.fontsReady, true);
    assert.equal(manifest.settings.playwright, "1.63.0");
    assert.match(manifest.settings.disableMotionCss, /animation-duration: 0s/);
    assert.deepEqual(manifest.viewport, { width: 1440, height: 900 });
    const png = await readFile(join(dir, "card.png"));
    assert.equal(manifest.image.sha256, sha256(png));
  });

  test("an ambiguous selector fails", async () => {
    await assert.rejects(
      capture({ url: `${origin}/card.html`, selector: ".card__tag", viewport: { width: 1440, height: 900 } }),
      (error: unknown) => error instanceof SelectorMatchError && error.count === 3 && /ambiguous/.test(error.message),
    );
  });

  test("a zero-match selector fails", async () => {
    await assert.rejects(
      capture({ url: `${origin}/card.html`, selector: ".no-such-thing", viewport: { width: 1440, height: 900 } }),
      (error: unknown) => error instanceof SelectorMatchError && error.count === 0,
    );
  });
});

describe("compare", () => {
  for (const viewport of VIEWPORTS) {
    test(`composite and manifest at ${viewport.width}px; wrong padding is highlighted and scores lower`, async () => {
      const dir = join(work, `flow-${viewport.width}`);
      const shots = join(dir, "shots");
      // The "Figma render" stand-in is a capture of the correct page.
      for (const [name, page] of [
        ["figma", "card.html"],
        ["good", "card.html"],
        ["wrong", "card-wrong-padding.html"],
      ] as const) {
        await captureToDir({ url: `${origin}/${page}`, selector: ".card", viewport, outDir: shots, name });
      }
      const out = join(dir, "out");
      const { manifest, manifestPath } = await compareToDir({
        figma: join(shots, "figma.png"),
        before: join(shots, "good.png"),
        after: join(shots, "wrong.png"),
        outDir: out,
      });

      assert.deepEqual(manifest.viewport, { width: viewport.width, height: viewport.height });
      assert.deepEqual(manifest.legend.compositeOrder, ["figma", "before", "after", "difference"]);
      assert.equal(manifest.score.before.score, 100);
      assert.ok(manifest.score.after.score < 100, `wrong padding scored ${manifest.score.after.score}`);
      assert.ok(manifest.score.after.mismatched > 0);
      assert.equal(manifest.inputs.after.capture?.url, `${origin}/card-wrong-padding.html`);

      // Every listed image exists with the recorded digest and size.
      const listed = [...Object.values(manifest.images), ...Object.values(manifest.inputs)];
      for (const image of listed) {
        const { bytes, image: decoded } = await readPng(join(out, image.file));
        assert.equal(sha256(bytes), image.sha256, image.file);
        assert.equal(decoded.width, image.width);
        assert.equal(decoded.height, image.height);
      }

      // The difference panel visibly highlights the wrong padding.
      const diff = (await readPng(join(out, "diff-after.png"))).image;
      let highlighted = 0;
      for (let i = 0; i < diff.data.length; i += 4) {
        const [r, g, b] = [diff.data[i], diff.data[i + 1], diff.data[i + 2]];
        if (r === 255 && g === 0 && (b === 0 || b === 255)) highlighted++;
      }
      assert.equal(highlighted, manifest.score.after.mismatched);
      const clean = (await readPng(join(out, "diff-before.png"))).image;
      assert.ok(clean.data.every((value, i) => i % 4 !== 0 || value !== 255 || clean.data[i + 1] !== 0));

      // The composite holds four panels side by side.
      assert.ok(manifest.images.composite.width > 4 * manifest.images.diffAfter.width);

      // Re-running the comparison reproduces the manifest byte for byte.
      const again = await compareToDir({
        figma: join(shots, "figma.png"),
        before: join(shots, "good.png"),
        after: join(shots, "wrong.png"),
        outDir: join(dir, "again"),
      });
      assert.equal(await readFile(again.manifestPath, "utf8"), await readFile(manifestPath, "utf8"));
    });
  }
});

describe("cli", () => {
  const run = promisify(execFile);

  test("capture and compare commands write manifests", async () => {
    const dir = join(work, "cli");
    const args = ["capture", "--url", `${origin}/card.html`, "--selector", ".card", "--viewport", "390x844", "--out", dir];
    await run(process.execPath, [CLI, ...args, "--name", "figma"]);
    await run(process.execPath, [CLI, ...args, "--name", "after"]);
    const { stdout } = await run(process.execPath, [
      CLI,
      "compare",
      "--figma",
      join(dir, "figma.png"),
      "--before",
      join(dir, "figma.png"),
      "--after",
      join(dir, "after.png"),
      "--out",
      join(dir, "out"),
    ]);
    const manifest = JSON.parse(await readFile(stdout.trim(), "utf8")) as CompareManifest;
    assert.equal(manifest.score.after.score, 100);
  });

  test("an ambiguous selector exits 1 with a message", async () => {
    await assert.rejects(
      run(process.execPath, [
        CLI,
        ...["capture", "--url", `${origin}/card.html`, "--selector", "li", "--viewport", "1440x900", "--out", join(work, "x")],
      ]),
      (error: unknown) => {
        const e = error as { code: number; stderr: string };
        return e.code === 1 && /ambiguous/.test(e.stderr);
      },
    );
  });

  test("a malformed viewport is rejected", async () => {
    await assert.rejects(
      run(process.execPath, [
        CLI,
        ...["capture", "--url", `${origin}/card.html`, "--selector", ".card", "--viewport", "wide", "--out", join(work, "y")],
      ]),
      (error: unknown) => (error as { code: number }).code === 1,
    );
  });
});
