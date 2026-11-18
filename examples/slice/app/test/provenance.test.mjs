// Stand-in provenance checks use temporary synthetic inputs; no browser needed.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { CARD_NODE, generateInputs, gitIdentity, repoPath, sha256, validateStandin, VIEWPORTS } from "../scripts/lib.mjs";

test("generated CSS and spec changes mark an initially clean checkout dirty", async () => {
  const repo = await mkdtemp(join(tmpdir(), "figmog-generated-identity-"));
  const git = (...args) => execFileSync("git", args, { cwd: repo, stdio: "pipe" });
  try {
    const tokensCss = join(repo, "examples", "slice", "app", "src", "tokens.css");
    const specJson = join(repo, "examples", "slice", "spec", "card.spec.json");
    await mkdir(join(tokensCss, ".."), { recursive: true });
    await mkdir(join(specJson, ".."), { recursive: true });
    await writeFile(tokensCss, "old CSS");
    await writeFile(specJson, "old spec");
    git("init");
    git("add", ".");
    git("-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Synthetic baseline");
    const initial = gitIdentity(repo);
    assert.equal(initial.dirty, false);
    const m = {
      status: () => ({ version: "4243" }),
      exportTokens: (path) => writeFileSync(path, "new CSS"),
      spec: () => "new spec",
    };
    const { identity } = await generateInputs(m, { tokensCss, specJson, repo });
    assert.deepEqual(identity, { ...initial, dirty: true });
    assert.equal(await readFile(tokensCss, "utf8"), "new CSS");
    assert.equal(await readFile(specJson, "utf8"), "new spec");
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test("stand-in provenance rejects changed inputs even at the same Figma version", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "figmog-standin-provenance-"));
  try {
    const figmaFile = join(root, "card-file.json");
    const source = join(root, "standin", "index.html");
    await mkdir(join(root, "standin"));
    await writeFile(figmaFile, JSON.stringify({ version: "4243", document: { name: "Synthetic card" } }));
    await writeFile(source, "<div>Synthetic card</div>");
    const digest = async (path) => ({ path: repoPath(path), sha256: sha256(await readFile(path)) });
    const record = {
      kind: "figma-render-stand-in",
      node: CARD_NODE,
      file: { ...(await digest(figmaFile)), version: "4243" },
      source: repoPath(source),
      sourceSha256: sha256(await readFile(source)),
      images: {},
    };
    for (const { width } of VIEWPORTS) {
      const path = join(root, `card-render-${width}.png`);
      // Validation checks the captured bytes, independent of PNG decoding.
      await writeFile(path, `synthetic image ${width}`);
      record.images[String(width)] = await digest(path);
    }
    const validate = (value = record) => validateStandin(value, { version: "4243" }, { figmaFile, standinDir: root });
    await t.test("accepts the recorded inputs", async () => {
      await validate();
    });
    for (const [label, path, message] of [
      ["same-version Figma file edit", figmaFile, /does not match the synthetic Figma file/],
      ["HTML source edit", source, /does not match its HTML source/],
      ...VIEWPORTS.map(({ width }) => [`${width}px PNG replacement`, join(root, `card-render-${width}.png`), new RegExp(`does not match its ${width}px PNG`)]),
    ]) {
      await t.test(`rejects a ${label}`, async () => {
        const original = await readFile(path);
        try {
          await writeFile(path, `${original.toString()} changed`);
          await assert.rejects(validate(), message);
        } finally {
          await writeFile(path, original);
        }
      });
    }
    await t.test("rejects legacy records without file provenance", async () => {
      const legacy = structuredClone(record);
      delete legacy.file.sha256;
      await assert.rejects(validate(legacy), /re-render it/);
    });
    await t.test("rejects a different version", async () => {
      await assert.rejects(validate({ ...record, file: { ...record.file, version: "4242" } }), /version 4242.*4243/);
    });
    await t.test("rejects another node", async () => {
      await assert.rejects(validate({ ...record, node: "2:1" }), /unexpected kind or node/);
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
