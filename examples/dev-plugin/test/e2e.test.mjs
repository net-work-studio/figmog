// End to end (NWS-2957): the sample app on a real Vite dev server with the
// figmog plugin, the bridge answering through a real `figmog serve` relay and
// the repository's background owner, the target runner, and a production
// build. Needs `cargo build` at the repository root (or FIGMOG_BIN) and
// `npm ci && npm run build` in packages/react.
//
// The app is copied into a fresh `git init` directory, so its mirror root
// (<git-common-dir>/figmog) and its background owner belong to this test
// alone, never to the checkout the test runs in.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { cp, mkdtemp, readdir, readFile, rm, symlink } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { chromium } from "playwright";
import { build, createLogger, createServer } from "vite";
import { config } from "../vite.config.js";

const APP = fileURLToPath(new URL("..", import.meta.url));
const REPO = join(APP, "..", "..");
const FIXTURE = join(REPO, "tests", "fixtures", "map", "pricing.json");
const SET = "component_set:5e7000000000000000000000000000000000a001";
const STAR = "component:5e7000000000000000000000000000000000c001";
const BADGE = "component:5e7000000000000000000000000000000000b001";

function figmogBin() {
  const bin = process.env["FIGMOG_BIN"] || join(REPO, "target", "debug", "figmog");
  if (!existsSync(bin)) throw new Error(`${bin} is missing: run \`cargo build\` at the repository root first`);
  return bin;
}

async function shot() {
  const entry = join(REPO, "packages", "react", "dist", "src", "index.js");
  if (!existsSync(entry)) throw new Error(`${entry} is missing: run \`npm ci && npm run build\` in packages/react`);
  return import(pathToFileURL(entry).href);
}

const BIN = figmogBin();
const warnings = [];
const logger = createLogger("warn", { allowClearScreen: false });
logger.warn = (message) => warnings.push(message);
logger.warnOnce = logger.warn;

let project = "";
let server;
let origin = "";
let port = 0;
let browser;
let page;

/** The sample app's own config, rooted at the test's copy of the app. */
function inline(extra = {}) {
  return {
    ...config({ figmog: BIN, serveArgs: ["--no-watch", "--no-upstream"] }),
    configFile: false,
    customLogger: logger,
    root: project,
    ...extra,
  };
}

before(async () => {
  project = await mkdtemp(join(tmpdir(), "dev-plugin-sample-"));
  for (const entry of ["figmog.map.json", "figmog.targets.json", "figmog.toml", "index.html", "src"]) {
    await cp(join(APP, entry), join(project, entry), { recursive: true });
  }
  await symlink(join(APP, "node_modules"), join(project, "node_modules"), "dir");
  execFileSync("git", ["init", "--quiet"], { cwd: project });
  // The synthetic Figma file, mirrored offline as figmog.toml's default file.
  execFileSync(BIN, ["pull", "--from-file", FIXTURE], { cwd: project, stdio: ["ignore", "ignore", "pipe"] });
  // The background owner the relay starts inherits this: it exits soon
  // after the dev server (its only client) closes.
  process.env["FIGMOG_OWNER_IDLE_MS"] = "500";

  server = await createServer(inline({ server: { host: "127.0.0.1", port: 0 } }));
  await server.listen();
  port = server.httpServer.address().port;
  origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({ headless: true });
  page = await browser.newPage();
  await page.goto(`${origin}/`, { waitUntil: "load" });
  await page.locator('[data-testid="pricing-pro"]').waitFor();
});

after(async () => {
  await browser?.close();
  await server?.close();
  // Wait for the owner to idle out and remove its discovery record.
  const record = join(project, ".git", "figmog", "owner.json");
  for (let i = 0; i < 300 && existsSync(record); i++) await new Promise((r) => setTimeout(r, 50));
  assert.ok(!existsSync(record), "the background owner exited after the dev server closed");
  await rm(project, { recursive: true, force: true });
});

describe("development server", () => {
  test("mapped components carry data-figmog attributes; a fragment root is skipped with a warning", async () => {
    const marked = await page.evaluate(() =>
      [...document.querySelectorAll("[data-figmog]")].map((el) => [
        el.tagName.toLowerCase(),
        el.getAttribute("data-figmog"),
        el.getAttribute("data-figmog-node"),
        el.getAttribute("data-testid"),
      ]),
    );
    assert.deepEqual(marked, [
      ["article", SET, "10:1", "pricing-pro"],
      ["svg", STAR, "13:1", null],
      ["article", SET, "10:1", "pricing-free"],
      ["svg", STAR, "13:1", null],
    ]);
    assert.equal(await page.locator(`[data-figmog="${BADGE}"]`).count(), 0);
    assert.equal(await page.getByText("Popular").count(), 1, "Badge still renders");
    assert.ok(
      warnings.some((w) => /not marking Badge .*fragment/.test(w)),
      `the skipped Badge is reported:\n${warnings.join("\n")}`,
    );
  });

  test("the bridge answers a figmog read from the dev page", async () => {
    const answer = await page.evaluate(async () => {
      const { url, token } = window.__FIGMOG_BRIDGE__;
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", "x-figmog-token": token },
        body: JSON.stringify({ tool: "figmog_node", arguments: { id: "10:1" } }),
      });
      return { status: response.status, body: await response.json() };
    });
    assert.equal(answer.status, 200, JSON.stringify(answer.body));
    assert.notEqual(answer.body.isError, true, JSON.stringify(answer.body));
    const node = JSON.parse(answer.body.content[0].text);
    assert.equal(node.name, "Pricing card");
    assert.equal(node.type, "COMPONENT_SET");

    // The spec of an instance names the mapped code component: the relay
    // injected the project's figmog.map.json.
    const spec = await page.evaluate(async () => {
      const { url, token } = window.__FIGMOG_BRIDGE__;
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", "x-figmog-token": token },
        body: JSON.stringify({ tool: "figmog_spec", arguments: { id: "1:2" } }),
      });
      return response.json();
    });
    assert.match(spec.content[0].text, /<PricingCard /);
  });

  test("the bridge refuses other origins, writes and other projects' files", async () => {
    const refused = await page.evaluate(async (other) => {
      const { url, token } = window.__FIGMOG_BRIDGE__;
      const post = (target, body) =>
        fetch(target, {
          method: "POST",
          headers: { "content-type": "application/json", "x-figmog-token": token },
          body: JSON.stringify(body),
        }).then(
          (r) => r.status,
          () => "blocked",
        );
      return {
        // The same server under another origin (Vite's own CORS allows every
        // localhost origin): the bridge refuses it and strips CORS headers,
        // so the page cannot even read the refusal.
        crossOrigin: await post(`http://localhost:${location.port}${url}`, { tool: "figmog_status" }),
        sync: await post(url, { tool: "figmog_sync" }),
        desktop: await post(url, { tool: "get_screenshot" }),
        otherFile: await post(url, { tool: "figmog_status", arguments: { file: other } }),
      };
    }, "SomeoneElsesFile9");
    assert.deepEqual(refused, { crossOrigin: "blocked", sync: 403, desktop: 403, otherFile: 403 });

    const token = await page.evaluate(() => window.__FIGMOG_BRIDGE__.token);
    const raw = (headers) =>
      new Promise((resolve, reject) => {
        const req = request({ host: "127.0.0.1", port, method: "POST", path: "/__figmog/bridge", headers }, (res) => {
          res.resume();
          resolve(res.statusCode);
        });
        req.on("error", reject);
        req.end('{"tool":"figmog_status"}');
      });
    const base = { "content-type": "application/json", host: `127.0.0.1:${port}`, origin, "x-figmog-token": token };
    assert.equal(await raw(base), 200);
    assert.equal(await raw({ ...base, "x-figmog-token": "wrong" }), 401);
    assert.equal(await raw({ ...base, origin: `http://localhost:${port}` }), 403);
    assert.equal(await raw({ ...base, host: `192.168.0.5:${port}`, origin: `http://192.168.0.5:${port}` }), 403);
  });

  test("the runner returns a PNG of one component through its explicit locator", async () => {
    const { captureTarget, capture, decodePng, SelectorMatchError } = await shot();
    const outDir = join(project, "shots");
    const result = await captureTarget({
      baseUrl: origin,
      manifest: join(project, "figmog.targets.json"),
      outDir,
      target: "pricing-pro",
    });
    assert.equal(result.selector, '[data-testid="pricing-pro"]');
    assert.deepEqual(
      result.captures.map((c) => c.viewport.width),
      [1440, 390],
    );
    for (const c of result.captures) {
      const png = decodePng(await readFile(c.pngPath));
      assert.equal(png.width, 288, "one 18rem card, not the page");
      assert.ok(png.height > 0 && png.height < 400);
    }
    // The automatic attribute names the component, not one instance of it.
    await assert.rejects(
      capture({ url: `${origin}/`, selector: '[data-figmog-node="10:1"]', viewport: { width: 1440, height: 900 } }),
      (e) => e instanceof SelectorMatchError && e.count === 2,
    );
  });
});

describe("production build", () => {
  test("contains nothing from figmog", async () => {
    const outDir = join(project, "dist");
    await build(inline({ build: { outDir, emptyOutDir: true }, logLevel: "silent" }));
    const token = await page.evaluate(() => window.__FIGMOG_BRIDGE__.token);
    const files = (await readdir(outDir, { recursive: true, withFileTypes: true })).filter((f) => f.isFile());
    assert.ok(files.some((f) => f.name.endsWith(".js")) && files.some((f) => f.name === "index.html"));
    for (const file of files) {
      const text = await readFile(join(file.parentPath, file.name), "utf8");
      assert.doesNotMatch(text, /figmog/i, `${file.name} mentions figmog`);
      assert.ok(!text.includes(token), `${file.name} contains the session token`);
    }
    // ... while the same app renders the same cards.
    const html = await readFile(join(outDir, "index.html"), "utf8");
    assert.match(html, /<div id="root"><\/div>/);
  });
});

test("the bridge's figmog is this project's background owner", async () => {
  const root = join(project, ".git", "figmog");
  const record = JSON.parse(await readFile(join(root, "owner.json"), "utf8"));
  assert.equal(record.project, realpathSync(root));
});
