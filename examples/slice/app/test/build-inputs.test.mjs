// Tailwind must consume only sources covered by the recorded build identity.
import assert from "node:assert/strict";
import { readdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { test } from "node:test";
import { buildApp, treeDigest, variantTree } from "../scripts/lib.mjs";

async function css(root) {
  const assets = join(await buildApp(root), "assets");
  const files = (await readdir(assets)).filter((file) => file.endsWith(".css")).sort();
  assert.ok(files.length > 0, "build produced no CSS");
  return (await Promise.all(files.map((file) => readFile(join(assets, file), "utf8")))).join("\n");
}

test("rebuilding an unchanged temporary tree produces identical CSS", async () => {
  const root = await variantTree();
  try {
    const digest = await treeDigest(root);
    const first = await css(root);
    assert.equal(await css(root), first, "the previous dist output became a Tailwind input");
    assert.deepEqual(await treeDigest(root), digest);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("Tailwind scans hashed sources and ignores files outside them", async () => {
  const root = await variantTree();
  try {
    const digest = await treeDigest(root);
    const first = await css(root);
    await writeFile(join(root, "extra.jsx"), '<div className="p-[137px]" />');
    assert.deepEqual(await treeDigest(root), digest, "extra.jsx is outside the build identity");
    await rm(join(root, "dist"), { recursive: true, force: true });
    assert.equal(await css(root), first, "an unhashed file changed the generated CSS");

    // Prove explicit scanning still includes source files, even when the app
    // does not import them. These files are copied and hashed by variantTree.
    await writeFile(join(root, "src", "extra.jsx"), '<div className="p-[139px]" />');
    assert.notDeepEqual(await treeDigest(root), digest);
    await rm(join(root, "dist"), { recursive: true, force: true });
    const scoped = await css(root);
    assert.notEqual(scoped, first);
    assert.match(scoped, /padding:139px/);
    assert.doesNotMatch(scoped, /padding:137px/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
