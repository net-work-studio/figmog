#!/usr/bin/env node
// The vertical slice (NWS-2965), end to end, offline:
//
//   node scripts/slice.mjs [run] [--link-base <url>]
//                                   synthetic Figma file -> token CSS -> spec ->
//                                   before/after/broken builds -> evidence
//                                   (--link-base: absolute COMMENT.md image links,
//                                   e.g. https://github.com/<owner>/<repo>/blob/<sha>)
//   node scripts/slice.mjs standin  re-render the committed Figma stand-in PNGs
//
// Run through ../run.sh, which builds figmog and packages/react first.
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  APP,
  buildApp,
  captureCard,
  CARD_NODE,
  CARDS,
  EVIDENCE_DIR,
  evidenceLink,
  FIGMA_FILE,
  generateInputs,
  mirror,
  REPO,
  repoPath,
  SELECTOR,
  serve,
  sha256,
  shot,
  SNAPSHOT,
  SPEC_JSON,
  STANDIN_DIR,
  standinPng,
  TOKENS_CSS,
  treeDigest,
  validateStandin,
  variantTree,
  VIEWPORTS,
  withBuild,
} from "./lib.mjs";

const STANDIN_RECORD = join(STANDIN_DIR, "card-render.json");
const STANDIN_CAVEAT =
  "STAND-IN, not a Figma export: figma/standin/index.html re-draws node 1:1 of figma/card-file.json from its raw values " +
  "in plain CSS and is captured with figmog-shot in the same Chromium as the app. No Figma access was used.";

async function digestFile(path) {
  return { path: repoPath(path), sha256: sha256(await readFile(path)) };
}

/** `slice.mjs standin`: capture figma/standin/index.html as the Figma render stand-in. */
async function standin() {
  const { stableJson } = await shot();
  const m = await mirror();
  let status;
  try {
    status = m.status();
  } finally {
    await m.cleanup();
  }
  const server = await serve(join(STANDIN_DIR, "standin"));
  const images = {};
  try {
    for (const viewport of VIEWPORTS) {
      const png = await captureCard({ origin: server.origin, viewport, outDir: STANDIN_DIR, name: `card-render-${viewport.width}` });
      images[String(viewport.width)] = await digestFile(png);
    }
  } finally {
    await server.close();
  }
  const record = {
    caveat: STANDIN_CAVEAT,
    file: { ...(await digestFile(FIGMA_FILE)), name: status.name, version: status.version },
    images,
    kind: "figma-render-stand-in",
    node: CARD_NODE,
    source: repoPath(join(STANDIN_DIR, "standin", "index.html")),
    sourceSha256: sha256(await readFile(join(STANDIN_DIR, "standin", "index.html"))),
  };
  await writeFile(STANDIN_RECORD, stableJson(record));
  process.stdout.write(`${repoPath(STANDIN_RECORD)}\n`);
}

/** Captures one served build at every viewport into `dir/<width>/<name>.png`. */
async function captureAll(origin, dir, name) {
  const out = {};
  for (const viewport of VIEWPORTS) {
    out[viewport.width] = await captureCard({ origin, viewport, outDir: join(dir, String(viewport.width)), name });
  }
  return out;
}

function formatScore(n) {
  return n.toFixed(2);
}

function comment(evidence, linkBase) {
  const lines = [];
  const link = (image) => evidenceLink(join(REPO, image.path), linkBase);
  const { builds, figma, runs, tokens } = evidence;
  lines.push("## Vertical slice evidence (NWS-2965)", "");
  lines.push(
    "One token snapshot -> one component -> before/after evidence, on a **synthetic** Figma file and a sample React + Tailwind v4 app.",
    "",
    `> **Figma render is a stand-in.** ${figma.render.caveat} The high "after" score is therefore expected by construction; ` +
      "it shows the pipeline works, not fidelity to a real Figma raster.",
    "",
  );
  lines.push("| | 1440 px | 390 px |", "| --- | --- | --- |");
  const row = (label, run, key) =>
    `| ${label} | ${formatScore(runs[run]["1440"].score[key])} | ${formatScore(runs[run]["390"].score[key])} |`;
  lines.push(row("before (baseline placeholder)", "correct", "before"));
  lines.push(row("**after (correct card)**", "correct", "after"));
  lines.push(row("broken card (wrong padding)", "broken", "after"));
  lines.push("", "Score: advisory, `100 * (1 - mismatched / (W*H))` against the Figma render; never a pass/fail gate.", "");
  for (const width of ["1440", "390"]) {
    lines.push(`### ${width} px: Figma | before | after | difference`, "");
    const images = runs.correct[width].images;
    lines.push(`![composite ${width}](${link(images.composite)})`, "");
    lines.push(
      `Figma: ![figma](${link(images.figma)}) before: ![before](${link(images.before)}) ` +
        `after: ![after](${link(images.after)}) difference: ![diff](${link(images.diffAfter)})`,
      "",
    );
  }
  lines.push("### Broken card (wrong padding), for contrast", "");
  for (const width of ["1440", "390"]) {
    lines.push(`${width} px: ![broken composite ${width}](${link(runs.broken[width].images.composite)})`, "");
  }
  lines.push("### Identity", "");
  lines.push(`- Figma file: \`${figma.file.path}\` "${figma.file.name}", version **${figma.file.version}** (mirrored with \`figmog pull --from-file\`)`);
  lines.push(`- Figma render: stand-in for version ${figma.render.version}, \`${figma.render.record}\``);
  lines.push(`- Token snapshot: \`${tokens.snapshot.path}\` digest \`${tokens.snapshot.digest}\` (exported_at ${tokens.snapshot.exportedAt})`);
  lines.push(`- Token CSS: \`${tokens.css.path}\` sha256 \`${tokens.css.sha256.slice(0, 16)}\``);
  for (const name of ["before", "after", "broken"]) {
    const b = builds[name];
    lines.push(
      `- Build \`${name}\`: card \`${b.card}\`, commit \`${b.commit.slice(0, 12)}\`${b.dirty ? " (dirty)" : ""}, ` +
        `tree sha256 \`${b.tree.sha256.slice(0, 16)}\` (${b.tree.files} files)`,
    );
  }
  lines.push(
    `- Contract: viewports ${evidence.contract.viewports.map((v) => `${v.width}x${v.height}`).join(", ")}; ` +
      `${evidence.contract.colorScheme} mode, ${evidence.contract.state} state, local only; selector \`${evidence.contract.selector}\``,
  );
  lines.push(`- Full record: \`${repoPath(join(EVIDENCE_DIR, "evidence.json"))}\``, "");
  return lines.join("\n");
}

/** The default command: the whole slice, writing the token CSS, spec and evidence/. */
async function run(linkBase) {
  const { compareToDir, stableJson } = await shot();

  // 1. Tokens and spec, from the synthetic file and the committed snapshot.
  const m = await mirror();
  let status;
  let identity;
  try {
    ({ status, identity } = await generateInputs(m));
  } finally {
    await m.cleanup();
  }
  const standinRecord = JSON.parse(await readFile(STANDIN_RECORD, "utf8"));
  await validateStandin(standinRecord, status);

  // 2. Builds and captures: before (baseline card), after (the app as is), broken.
  const work = await mkdtemp(join(tmpdir(), "figmog-slice-shots-"));
  const trees = [];
  try {
    const builds = {};
    const shots = {};
    for (const name of ["before", "after", "broken"]) {
      const root = name === "after" ? APP : await variantTree({ card: CARDS[name] });
      if (root !== APP) trees.push(root);
      builds[name] = { card: repoPath(join(APP, CARDS[name])), ...identity, tree: await treeDigest(root) };
      shots[name] = await withBuild(root, (origin) => captureAll(origin, join(work, name), name));
    }

    // 3. Compare against the stand-in Figma render.
    await rm(EVIDENCE_DIR, { recursive: true, force: true });
    const runs = { broken: {}, correct: {} };
    for (const [run, candidate] of [
      ["correct", "after"],
      ["broken", "broken"],
    ]) {
      for (const { width } of VIEWPORTS) {
        const outDir = join(EVIDENCE_DIR, run, String(width));
        const { manifest } = await compareToDir({
          after: shots[candidate][width],
          before: shots.before[width],
          figma: standinPng(width),
          outDir,
        });
        const img = (file) => ({ path: repoPath(join(outDir, file)) });
        const images = {};
        for (const [key, file] of Object.entries({
          after: "after.png",
          before: "before.png",
          composite: "composite.png",
          diffAfter: "diff-after.png",
          diffBefore: "diff-before.png",
          figma: "figma.png",
        })) {
          images[key] = { ...img(file), sha256: sha256(await readFile(join(outDir, file))) };
        }
        runs[run][width] = {
          images,
          manifest: repoPath(join(outDir, "manifest.json")),
          score: { after: manifest.score.after.score, before: manifest.score.before.score },
        };
      }
    }

    const evidence = {
      builds,
      contract: {
        colorScheme: "light",
        local: true,
        selector: SELECTOR,
        state: "default",
        viewports: VIEWPORTS,
      },
      figma: {
        file: { ...(await digestFile(FIGMA_FILE)), name: status.name, version: status.version },
        node: CARD_NODE,
        render: {
          caveat: standinRecord.caveat,
          images: standinRecord.images,
          kind: standinRecord.kind,
          record: repoPath(STANDIN_RECORD),
          source: standinRecord.source,
          sourceSha256: standinRecord.sourceSha256,
          version: standinRecord.file.version,
        },
      },
      kind: "figmog-slice/evidence",
      runs,
      scoreNote: "advisory: 100 * (1 - mismatched / (W*H)) against the Figma render; never a pass/fail gate",
      spec: await digestFile(SPEC_JSON),
      tokens: {
        css: await digestFile(TOKENS_CSS),
        snapshot: {
          ...(await digestFile(SNAPSHOT)),
          digest: status.variables.digest,
          exportedAt: status.variables.exported_at,
        },
      },
      version: 1,
    };
    await writeFile(join(EVIDENCE_DIR, "evidence.json"), stableJson(evidence));
    await writeFile(join(EVIDENCE_DIR, "COMMENT.md"), comment(evidence, linkBase));
    process.stdout.write(
      `${stableJson({
        broken: { 1440: runs.broken["1440"].score.after, 390: runs.broken["390"].score.after },
        correct: { 1440: runs.correct["1440"].score, 390: runs.correct["390"].score },
        evidence: repoPath(join(EVIDENCE_DIR, "evidence.json")),
      })}`,
    );
  } finally {
    await rm(work, { recursive: true, force: true });
    for (const tree of trees) await rm(tree, { recursive: true, force: true });
  }
}

/** `[run|standin] [--link-base <url>]`; null on a usage error. */
function parseArgs(argv) {
  const args = [...argv];
  let linkBase;
  const at = args.indexOf("--link-base");
  if (at !== -1) {
    linkBase = args[at + 1];
    if (!linkBase || linkBase.startsWith("-")) return null;
    args.splice(at, 2);
  }
  const command = args[0] ?? "run";
  if (args.length > 1) return null;
  if (command === "run") return () => run(linkBase);
  if (command === "standin" && linkBase === undefined) return standin;
  return null;
}

const main = parseArgs(process.argv.slice(2));
if (main === null) {
  process.stderr.write("usage: node scripts/slice.mjs [run [--link-base <url>] | standin]\n");
  process.exit(2);
}
main().catch((error) => {
  process.stderr.write(`slice: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`);
  process.exit(1);
});
