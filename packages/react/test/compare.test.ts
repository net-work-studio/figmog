import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { compareToDir, diffImages, sha256, stableJson, type CaptureManifest } from "../src/index.js";
import { decodePng, encodePng, solid } from "../src/png.js";

test("identical images score 100 with no mismatches", () => {
  const a = solid(4, 3, [10, 20, 30, 255]);
  const result = diffImages(a, solid(4, 3, [10, 20, 30, 255]));
  assert.equal(result.mismatched, 0);
  assert.equal(result.total, 12);
  assert.equal(result.score, 100);
});

test("size mismatch pads to the larger size and counts padding as mismatch", () => {
  // 4x3 vs 2x4: canvas 4x4 = 16 px, common area 2x3 = 6 px, so 10 px only one (or neither) image covers.
  const result = diffImages(solid(4, 3, [0, 0, 0, 255]), solid(2, 4, [0, 0, 0, 255]));
  assert.equal(result.image.width, 4);
  assert.equal(result.image.height, 4);
  assert.equal(result.mismatched, 10);
  assert.equal(result.score, 37.5);
  // Bottom-right pixel is padding: magenta.
  assert.deepEqual([...result.image.data.subarray(15 * 4, 16 * 4)], [255, 0, 255, 255]);
});

test("differing pixels are counted and painted red", () => {
  const a = solid(10, 10, [255, 255, 255, 255]);
  const b = solid(10, 10, [255, 255, 255, 255]);
  b.data.set([0, 0, 0, 255], (5 * 10 + 5) * 4);
  const result = diffImages(a, b);
  assert.equal(result.mismatched, 1);
  assert.equal(result.score, 99);
  assert.deepEqual([...result.image.data.subarray((5 * 10 + 5) * 4, (5 * 10 + 6) * 4)], [255, 0, 0, 255]);
});

test("PNG encoding round-trips and is byte-stable", () => {
  const a = solid(3, 2, [1, 2, 3, 4]);
  const bytes = encodePng(a);
  assert.deepEqual(bytes, encodePng(a));
  assert.deepEqual(decodePng(bytes), a);
});

test("stableJson sorts keys at every level", () => {
  assert.equal(stableJson({ b: 1, a: { d: [{ z: 1, y: 2 }], c: null } }), stableJson({ a: { c: null, d: [{ y: 2, z: 1 }] }, b: 1 }));
  assert.equal(stableJson({ b: 1, a: 2 }), '{\n  "a": 2,\n  "b": 1\n}\n');
});

/** Synthetic capture records let provenance checks run without a browser. */
async function comparisonInputs(dir: string) {
  const inputs = {} as Record<"figma" | "before" | "after", string>;
  const manifests = {} as Record<"figma" | "before" | "after", CaptureManifest>;
  const bytes = encodePng(solid(4, 3, [10, 20, 30, 255]));
  for (const name of ["figma", "before", "after"] as const) {
    inputs[name] = join(dir, `${name}.png`);
    const manifest: CaptureManifest = {
      image: { file: `${name}.png`, width: 4, height: 3, sha256: sha256(bytes) },
      kind: "figmog-shot/capture",
      selector: ".synthetic",
      settings: {
        animations: "disabled",
        browser: { args: [], name: "chromium", version: "synthetic" },
        caret: "hidden",
        colorScheme: "light",
        deviceScaleFactor: 1,
        disableMotionCss: "",
        fontsReady: true,
        headless: true,
        locale: "en-US",
        playwright: "1.63.0",
        reducedMotion: "reduce",
        timezoneId: "UTC",
      },
      url: `http://127.0.0.1/${name}`,
      version: 1,
      viewport: { width: 1440, height: 900 },
    };
    manifests[name] = manifest;
    await writeFile(inputs[name], bytes);
    await writeFile(join(dir, `${name}.manifest.json`), stableJson(manifest));
  }
  return { options: { ...inputs, outDir: join(dir, "out") }, manifests };
}

test("compare preserves verified capture metadata and a shared viewport", async () => {
  const dir = await mkdtemp(join(tmpdir(), "figmog-compare-provenance-"));
  try {
    const { options, manifests } = await comparisonInputs(dir);
    const { manifest } = await compareToDir(options);
    assert.deepEqual(manifest.viewport, { width: 1440, height: 900 });
    for (const name of ["figma", "before", "after"] as const) {
      assert.deepEqual(manifest.inputs[name].capture, manifests[name]);
      assert.equal(sha256(await readFile(join(options.outDir, `${name}.png`))), manifest.inputs[name].sha256);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("compare rejects a replaced PNG with its previous sidecar", async (t) => {
  for (const name of ["figma", "before", "after"] as const) {
    await t.test(name, async () => {
      const dir = await mkdtemp(join(tmpdir(), "figmog-compare-stale-"));
      try {
        const { options } = await comparisonInputs(dir);
        await writeFile(options[name], encodePng(solid(4, 3, [100, 20, 30, 255])));
        await assert.rejects(compareToDir(options), (error: unknown) =>
          error instanceof Error && error.message.includes(`${name}.manifest.json`) &&
          /image SHA-256 does not match the PNG/.test(error.message));
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    });
  }
});

test("compare rejects invalid capture sidecars even when their digest matches", async (t) => {
  const cases: [string, (manifest: CaptureManifest) => unknown, RegExp][] = [
    ["kind", (m) => ({ ...m, kind: "figmog-shot/compare" }), /expected figmog-shot\/capture version 1/],
    ["version", (m) => ({ ...m, version: 2 }), /expected figmog-shot\/capture version 1/],
    ["width", (m) => ({ ...m, image: { ...m.image, width: 5 } }), /image dimensions do not match the PNG/],
    ["height", (m) => ({ ...m, image: { ...m.image, height: 5 } }), /image dimensions do not match the PNG/],
    ["missing image", (m) => ({ ...m, image: null }), /image SHA-256 does not match the PNG/],
    ["missing viewport", (m) => ({ ...m, viewport: null }), /expected a viewport with positive integer dimensions/],
    ["invalid viewport", (m) => ({ ...m, viewport: { width: -1, height: 900 } }), /positive integer dimensions/],
    ["null", () => null, /expected figmog-shot\/capture version 1/],
  ];
  for (const [label, change, message] of cases) {
    await t.test(label, async () => {
      const dir = await mkdtemp(join(tmpdir(), "figmog-compare-invalid-"));
      try {
        const { options, manifests } = await comparisonInputs(dir);
        await writeFile(join(dir, "after.manifest.json"), stableJson(change(manifests.after)));
        await assert.rejects(compareToDir(options), message);
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    });
  }
});

test("compare rejects a capture sidecar containing invalid JSON", async () => {
  const dir = await mkdtemp(join(tmpdir(), "figmog-compare-json-"));
  try {
    const { options } = await comparisonInputs(dir);
    await writeFile(join(dir, "figma.manifest.json"), "{");
    await assert.rejects(compareToDir(options), /invalid capture sidecar .*figma\.manifest\.json: invalid JSON/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("compare accepts standalone PNGs without capture metadata", async () => {
  const dir = await mkdtemp(join(tmpdir(), "figmog-compare-plain-"));
  try {
    const { options } = await comparisonInputs(dir);
    for (const name of ["figma", "before", "after"]) await rm(join(dir, `${name}.manifest.json`));
    const { manifest } = await compareToDir(options);
    assert.equal(manifest.viewport, null);
    for (const input of Object.values(manifest.inputs)) assert.equal(input.capture, null);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("different verified capture viewports produce no shared viewport", async () => {
  const dir = await mkdtemp(join(tmpdir(), "figmog-compare-viewports-"));
  try {
    const { options, manifests } = await comparisonInputs(dir);
    manifests.after.viewport = { width: 390, height: 844 };
    await writeFile(join(dir, "after.manifest.json"), stableJson(manifests.after));
    const { manifest } = await compareToDir(options);
    assert.equal(manifest.viewport, null);
    assert.deepEqual(manifest.inputs.after.capture?.viewport, { width: 390, height: 844 });
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
