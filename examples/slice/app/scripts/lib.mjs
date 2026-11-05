// Shared plumbing for the vertical slice (NWS-2965): the figmog CLI against a
// throwaway store, variant builds of the app, a static server, and the
// figmog-shot API from packages/react. Used by scripts/slice.mjs and the
// tests; nothing here touches the network.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, readdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, normalize, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const APP = fileURLToPath(new URL("..", import.meta.url));
export const SLICE = join(APP, "..");
export const REPO = join(SLICE, "..", "..");

/** The synthetic Figma file (a saved GET /v1/files/:key response). */
export const FIGMA_FILE = join(SLICE, "figma", "card-file.json");
/**
 * The committed variables export: one source of truth, shared with the
 * `figmog spec` test fixture (tests/fixtures/spec/), whose variable ids the
 * card file binds to.
 */
export const SNAPSHOT = join(REPO, "tests", "fixtures", "spec", "variables.json");
/** `figmog tokens export` output the app imports (committed). */
export const TOKENS_CSS = join(APP, "src", "tokens.css");
/** `figmog spec 1:1` output the card was built from (committed). */
export const SPEC_JSON = join(SLICE, "spec", "card.spec.json");
export const CARD_NODE = "1:1";
export const STANDIN_DIR = join(SLICE, "figma");
export const EVIDENCE_DIR = join(SLICE, "evidence");

/** The selector every capture uses: the card and nothing else. */
export const SELECTOR = '[data-testid="slice-card"]';
/** First verification contract: light mode, default state, both widths. */
export const VIEWPORTS = [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
];

/** Card implementations, relative to the app directory. */
export const CARDS = {
  after: "src/Card.jsx",
  before: "baseline/Card.jsx",
  broken: "variants/Card.wrong-padding.jsx",
  hardcoded: "variants/Card.hardcoded.jsx",
};

/** Repository-relative, forward-slash path for records and the PR comment. */
export function repoPath(path) {
  return relative(REPO, path).split("\\").join("/");
}

/**
 * The link COMMENT.md uses for a repository file. With `linkBase` (e.g.
 * `https://github.com/<owner>/<repo>/blob/<sha>`): `<linkBase>/<path>?raw=true`,
 * which renders anywhere the comment is posted. Without: the path relative to
 * COMMENT.md's own directory (`correct/1440/composite.png`), which renders
 * when COMMENT.md is viewed in the repository.
 */
export function evidenceLink(path, linkBase) {
  if (linkBase) return `${linkBase.replace(/\/+$/, "")}/${repoPath(path)}?raw=true`;
  return relative(EVIDENCE_DIR, path).split("\\").join("/");
}

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

/** packages/react's figmog-shot API (build it first: `npm run build` there). */
export async function shot() {
  const entry = join(REPO, "packages", "react", "dist", "src", "index.js");
  if (!existsSync(entry)) {
    throw new Error(`${repoPath(entry)} is missing: run \`npm ci && npm run build\` in packages/react first`);
  }
  return import(pathToFileURL(entry).href);
}

/** The repository's own figmog binary (`cargo build`), or FIGMOG_BIN. */
export function figmogBin() {
  const bin = process.env["FIGMOG_BIN"] || join(REPO, "target", "debug", "figmog");
  if (!existsSync(bin)) throw new Error(`${bin} is missing: run \`cargo build\` at the repository root first`);
  return bin;
}

/**
 * Runs figmog against an explicit store. `--db` plus `--no-socket` means it
 * opens the store directly: no serve relay, no background owner process.
 */
export function figmog(db, args) {
  return execFileSync(figmogBin(), ["--no-socket", "--db", db, ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}

/**
 * A throwaway mirror: the synthetic Figma file pulled from disk and the
 * variables snapshot imported. Call `cleanup()` when done.
 */
export async function mirror({ figmaFile = FIGMA_FILE, snapshot = SNAPSHOT } = {}) {
  const dir = await mkdtemp(join(tmpdir(), "figmog-slice-db-"));
  const db = join(dir, "db");
  figmog(db, ["pull", "--from-file", figmaFile]);
  figmog(db, ["import-variables", snapshot]);
  return {
    db,
    status: () => JSON.parse(figmog(db, ["status"])),
    exportTokens: (out) => JSON.parse(figmog(db, ["tokens", "export", "--out", out])),
    spec: (node = CARD_NODE) => figmog(db, ["spec", node]),
    cleanup: () => rm(dir, { recursive: true, force: true }),
  };
}

/** Files that make up an app build, relative to its root, sorted. */
async function buildInputs(root) {
  const out = ["index.html", "package-lock.json", "package.json", "vite.config.js"];
  const walk = async (dir) => {
    for (const entry of await readdir(join(root, dir), { withFileTypes: true })) {
      const rel = `${dir}/${entry.name}`;
      if (entry.isDirectory()) await walk(rel);
      else out.push(rel);
    }
  };
  await walk("src");
  return out.sort();
}

/**
 * Digest of a build's source tree: SHA-256 over sorted `sha256  path` lines
 * of every build input (index.html, package files, vite config, src/**).
 */
export async function treeDigest(root) {
  const lines = [];
  for (const rel of await buildInputs(root)) lines.push(`${sha256(await readFile(join(root, rel)))}  ${rel}\n`);
  return { files: lines.length, sha256: sha256(lines.join("")) };
}

/** The checkout's commit and whether the slice's inputs differ from it. */
export function gitIdentity() {
  const git = (...args) => execFileSync("git", args, { cwd: REPO, encoding: "utf8" }).trim();
  const paths = ["examples/slice/app", "examples/slice/figma", "examples/slice/spec", "tests/fixtures/spec/variables.json"];
  return { commit: git("rev-parse", "HEAD"), dirty: git("status", "--porcelain", "--", ...paths) !== "" };
}

/**
 * A temporary copy of the app's build inputs with some files replaced: the
 * card (`card`, an app-relative path) and/or the token CSS (`tokensCss`, a
 * path). node_modules is linked, not copied. Returns the directory.
 */
export async function variantTree({ card, tokensCss } = {}) {
  const root = await mkdtemp(join(tmpdir(), "figmog-slice-app-"));
  for (const rel of await buildInputs(APP)) {
    await mkdir(join(root, rel, ".."), { recursive: true });
    await cp(join(APP, rel), join(root, rel));
  }
  if (card !== undefined) await cp(join(APP, card), join(root, "src", "Card.jsx"));
  if (tokensCss !== undefined) await cp(tokensCss, join(root, "src", "tokens.css"));
  await symlink(join(APP, "node_modules"), join(root, "node_modules"), "dir");
  return root;
}

/** `vite build` of `root` into `root/dist`; returns the output directory. */
export async function buildApp(root) {
  const { build } = await import("vite");
  const outDir = join(root, "dist");
  await build({ root, configFile: join(root, "vite.config.js"), logLevel: "warn", build: { outDir, emptyOutDir: true } });
  return outDir;
}

const TYPES = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
};

/** Serves `dir` on an ephemeral loopback port (`/` is index.html). */
export async function serve(dir) {
  const server = createServer((request, response) => {
    let path = normalize(decodeURIComponent(new URL(request.url ?? "/", "http://x").pathname));
    if (path.includes("..")) {
      response.writeHead(400).end();
      return;
    }
    if (path.endsWith("/")) path += "index.html";
    readFile(join(dir, path)).then(
      (body) => response.writeHead(200, { "content-type": TYPES[extname(path)] ?? "application/octet-stream" }).end(body),
      () => response.writeHead(404).end(),
    );
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  return {
    origin: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve, reject) => server.close((e) => (e ? reject(e) : resolve()))),
  };
}

/** Placeholder for the random port in recorded URLs, so records are stable. */
export const RECORDED_ORIGIN = "http://127.0.0.1:<port>";

/**
 * figmog-shot capture of the card, with the served URL's random port
 * replaced by a placeholder in the manifest so repeated runs record the same
 * bytes. Returns the PNG path.
 */
export async function captureCard({ origin, path = "/", viewport, outDir, name }) {
  const { captureToDir, stableJson } = await shot();
  const { manifest, manifestPath, pngPath } = await captureToDir({
    name,
    outDir,
    selector: SELECTOR,
    url: `${origin}${path}`,
    viewport,
  });
  manifest.url = `${RECORDED_ORIGIN}${path}`;
  await writeFile(manifestPath, stableJson(manifest));
  return pngPath;
}

/** Builds `root`, serves it, runs `fn(origin)`, and always stops the server. */
export async function withBuild(root, fn) {
  const server = await serve(await buildApp(root));
  try {
    return await fn(server.origin);
  } finally {
    await server.close();
  }
}

/** Stand-in render paths for one viewport width. */
export function standinPng(width) {
  return join(STANDIN_DIR, `card-render-${width}.png`);
}
