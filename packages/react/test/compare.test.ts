import assert from "node:assert/strict";
import { test } from "node:test";
import { diffImages, stableJson } from "../src/index.js";
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
