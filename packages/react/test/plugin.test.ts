// The Vite plugin and the bridge's security rules (NWS-2957), against a real
// Vite dev server and a stand-in figmog binary (test/fixtures/fake-figmog.mjs),
// so no Rust build is needed. The end-to-end run against the real figmog
// binary is examples/dev-plugin's test.
import assert from "node:assert/strict";
import { mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import type { AddressInfo } from "node:net";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { build, createLogger, createServer, type InlineConfig, type ViteDevServer } from "vite";
import { BRIDGE_PATH, fileKey, isLoopbackAddress, isLoopbackHost } from "../src/bridge.js";
import { figmog, type FigmogPluginOptions } from "../src/vite.js";
import { FIXTURES } from "./server.js";

const APP = join(FIXTURES, "app");
const FAKE = join(FIXTURES, "fake-figmog.mjs");
const PRODUCT = "FakeProductFile01";
const OTHER = "SomeoneElsesFile9";

const warnings: string[] = [];
const logger = createLogger("warn", { allowClearScreen: false });
logger.warn = (message) => {
  warnings.push(message);
};
logger.warnOnce = logger.warn;

function config(options: FigmogPluginOptions, extra: InlineConfig = {}): InlineConfig {
  return {
    configFile: false,
    customLogger: logger,
    logLevel: "warn",
    // No __source paths (they would name this checkout in the build).
    oxc: { jsx: { development: false, pragma: "h", runtime: "classic" } },
    plugins: [figmog({ figmog: FAKE, ...options })],
    root: APP,
    ...extra,
  };
}

interface Reply {
  status: number;
  body: string;
}

/** A raw HTTP request, so Host, Origin and the peer address can be anything. */
function send(
  port: number,
  options: { method?: string; path?: string; headers?: Record<string, string>; body?: string; address?: string },
): Promise<Reply> {
  return new Promise((resolve, reject) => {
    const req = httpRequest(
      {
        headers: options.headers,
        host: options.address ?? "127.0.0.1",
        method: options.method ?? "POST",
        path: options.path ?? BRIDGE_PATH,
        port,
      },
      (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (chunk: Buffer) => chunks.push(chunk));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, body: Buffer.concat(chunks).toString("utf8") }));
      },
    );
    req.on("error", reject);
    req.end(options.body);
  });
}

let server: ViteDevServer;
let port = 0;
let token = "";
let log = "";
let work = "";

/** Headers of a well-formed same-origin request from the dev page. */
function page(extra: Record<string, string> = {}): Record<string, string> {
  return {
    "content-type": "application/json",
    host: `localhost:${port}`,
    origin: `http://localhost:${port}`,
    "sec-fetch-site": "same-origin",
    "x-figmog-token": token,
    ...extra,
  };
}

function call(tool: string, args: Record<string, unknown> = {}, headers: Record<string, string> = {}): Promise<Reply> {
  return send(port, { body: JSON.stringify({ arguments: args, tool }), headers: page(headers) });
}

async function forwarded(): Promise<string[]> {
  try {
    return (await readFile(log, "utf8"))
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => (JSON.parse(line) as { params: { name: string } }).params.name);
  } catch {
    return [];
  }
}

before(async () => {
  work = await mkdtemp(join(tmpdir(), "figmog-plugin-"));
  log = join(work, "relay.log");
  process.env["FAKE_FIGMOG_LOG"] = log;
  process.env["FAKE_FIGMOG_ATTRIBUTES"] = "1";
  // Every interface (`vite --host`) and no Host allowlist of Vite's own, so
  // every refusal below is the bridge's.
  server = await createServer(config({}, { server: { allowedHosts: true, host: "0.0.0.0", port: 0 } }));
  await server.listen();
  port = (server.httpServer?.address() as AddressInfo).port;
  const html = await (await fetch(`http://localhost:${port}/`)).text();
  const match = /window\.__FIGMOG_BRIDGE__=Object\.freeze\((\{.*?\})\);/.exec(html);
  assert.ok(match?.[1], "the dev page carries the bridge token");
  const info = JSON.parse(match[1]) as { token: string; url: string };
  assert.equal(info.url, BRIDGE_PATH);
  token = info.token;
  assert.match(token, /^[A-Za-z0-9_-]{43}$/);
});

after(async () => {
  await server.close();
  delete process.env["FAKE_FIGMOG_LOG"];
  delete process.env["FAKE_FIGMOG_ATTRIBUTES"];
  await rm(work, { recursive: true, force: true });
});

describe("attributes", () => {
  test("mark mapped components' root elements in development, from settings", async () => {
    const card = await server.transformRequest("/src/Card.jsx");
    assert.ok(card);
    assert.match(card.code, /"section",\s*\{[^}]*"data-figmog": "component_set:5e7000000000000000000000000000000000a001"/);
    assert.match(card.code, /"data-figmog-node": "10:1"/);
    assert.match(card.code, /"data-figmog": "component:5e7000000000000000000000000000000000c001"/);
    assert.match(card.code, /"data-figmog-node": "13:1"/);
    const badge = await server.transformRequest("/src/Badge.jsx");
    assert.ok(badge);
    assert.doesNotMatch(badge.code, /data-figmog/);
    assert.ok(
      warnings.some((w) => /not marking Badge .*fragment/.test(w)),
      `a fragment root is reported: ${warnings.join("\n")}`,
    );
  });

  test("are off unless settings or options turn them on", async () => {
    delete process.env["FAKE_FIGMOG_ATTRIBUTES"];
    const off = await createServer(config({ bridge: false }, { server: { middlewareMode: true } }));
    try {
      const card = await off.transformRequest("/src/Card.jsx");
      assert.doesNotMatch(card?.code ?? "", /data-figmog/);
    } finally {
      await off.close();
    }
    const on = await createServer(config({ attributes: true, bridge: false }, { server: { middlewareMode: true } }));
    try {
      const card = await on.transformRequest("/src/Card.jsx");
      assert.match(card?.code ?? "", /data-figmog/);
    } finally {
      await on.close();
      process.env["FAKE_FIGMOG_ATTRIBUTES"] = "1";
    }
  });

  test("a production build contains nothing from figmog", async () => {
    const outDir = join(work, "dist");
    await build(config({ attributes: true }, { build: { outDir, emptyOutDir: true } }));
    const files = await readdir(outDir, { recursive: true, withFileTypes: true });
    const built = files.filter((f) => f.isFile());
    assert.ok(built.length >= 2, "index.html and a bundle were built");
    for (const file of built) {
      const text = await readFile(join(file.parentPath, file.name), "utf8");
      assert.doesNotMatch(text, /figmog/i, `${file.name} mentions figmog`);
    }
  });
});

describe("bridge", () => {
  test("answers an allowed read from the dev page, forwarding it unchanged", async () => {
    const reply = await call("figmog_node", { id: "10:1", file: PRODUCT });
    assert.equal(reply.status, 200, reply.body);
    const result = JSON.parse(reply.body) as { content: { text: string }[] };
    assert.deepEqual(JSON.parse(result.content[0]?.text ?? ""), {
      arguments: { file: PRODUCT, id: "10:1" },
      tool: "figmog_node",
    });
    for (const host of [`127.0.0.1:${port}`, `[::1]:${port}`]) {
      const ok = await call("figmog_status", {}, { host, origin: `http://${host}` });
      assert.equal(ok.status, 200, `${host}: ${ok.body}`);
    }
    const images = await call("figmog_images", { ids: ["10:1"] });
    assert.equal(images.status, 200, images.body);
  });

  test("refuses requests without the session token", async () => {
    for (const headers of [{ "x-figmog-token": "" }, { "x-figmog-token": `${token.slice(1)}x` }, { "x-figmog-token": "short" }]) {
      const reply = await call("figmog_status", {}, headers);
      assert.equal(reply.status, 401, reply.body);
    }
    const none = page();
    delete none["x-figmog-token"];
    assert.equal((await send(port, { body: '{"tool":"figmog_status"}', headers: none })).status, 401);
  });

  test("refuses a wrong Origin: another dev server, another site, none", async () => {
    for (const origin of [`http://localhost:${port + 1}`, `http://127.0.0.1:${port}`, "https://evil.example", "null"]) {
      const reply = await call("figmog_status", {}, { origin });
      assert.equal(reply.status, 403, `${origin}: ${reply.body}`);
    }
    const none = page();
    delete none["origin"];
    assert.equal((await send(port, { body: '{"tool":"figmog_status"}', headers: none })).status, 403);
    const crossSite = await call("figmog_status", {}, { "sec-fetch-site": "cross-site" });
    assert.equal(crossSite.status, 403);
  });

  test("refuses a Host that is not localhost (LAN address, DNS rebinding)", async () => {
    for (const host of [`192.168.1.20:${port}`, `evil.example:${port}`, `localhost.evil.example:${port}`, `0.0.0.0:${port}`]) {
      const reply = await call("figmog_status", {}, { host, origin: `http://${host}` });
      assert.equal(reply.status, 403, `${host}: ${reply.body}`);
      assert.match(reply.body, /only answers on localhost/);
    }
  });

  test("never lets another origin read an answer (Vite's CORS headers are removed)", async () => {
    const other = `http://localhost:${port + 1}`;
    const reply = await new Promise<{ status: number; cors: string | undefined }>((resolve, reject) => {
      const req = httpRequest(
        { host: "127.0.0.1", port, method: "POST", path: BRIDGE_PATH, headers: page({ origin: other }) },
        (res) => {
          res.resume();
          resolve({ status: res.statusCode ?? 0, cors: res.headers["access-control-allow-origin"] });
        },
      );
      req.on("error", reject);
      req.end('{"tool":"figmog_status"}');
    });
    assert.deepEqual(reply, { status: 403, cors: undefined });
  });

  test("refuses peers that are not this machine, even with forged headers", async (t) => {
    assert.ok(isLoopbackAddress("127.0.0.1") && isLoopbackAddress("::1") && isLoopbackAddress("::ffff:127.0.0.1"));
    assert.ok(!isLoopbackAddress("10.0.0.2") && !isLoopbackAddress("::ffff:192.168.1.2") && !isLoopbackAddress(undefined));
    assert.ok(isLoopbackHost("localhost:5173") && isLoopbackHost("[::1]:5173") && isLoopbackHost("127.0.0.1"));
    assert.ok(!isLoopbackHost("LOCALHOST:5173") && !isLoopbackHost("127.0.0.2:5173") && !isLoopbackHost("a.localhost:1"));
    // The server listens on every interface (like `vite --host`); reach it
    // through a non-loopback address of this machine, if it has one.
    const lan = Object.values(networkInterfaces())
      .flat()
      .find((a) => a !== undefined && a.family === "IPv4" && !a.internal);
    if (lan === undefined) {
      t.diagnostic("no non-loopback IPv4 interface on this machine: only the predicates were checked");
      return;
    }
    const reply = await send(port, { address: lan.address, body: '{"tool":"figmog_status"}', headers: page() });
    assert.equal(reply.status, 403, reply.body);
    assert.match(reply.body, /from this machine/);
  });

  test("refuses tools outside the read + render allowlist, before figmog sees them", async () => {
    const before = await forwarded();
    for (const tool of [
      "figmog_sync",
      "figmog_open",
      "figmog_files",
      "figmog_internal_pull",
      "figmog_internal_import_variables",
      "get_screenshot",
      "get_design_context",
      "tools/list",
    ]) {
      const reply = await call(tool);
      assert.equal(reply.status, 403, `${tool}: ${reply.body}`);
    }
    assert.equal((await call("figmog_images", { ids: ["1:1"], refresh: true })).status, 403);
    assert.deepEqual(await forwarded(), before, "nothing refused reached figmog");
  });

  test("refuses other projects' files", async () => {
    const before = await forwarded();
    const refused: Record<string, unknown>[] = [
      { file: OTHER },
      { file: `https://www.figma.com/design/${OTHER}/Name` },
      { file: "not a file" },
      { file: 42 },
      { id: `https://www.figma.com/design/${OTHER}/Name?node-id=1-2` },
      { under: `https://figma.com/file/${OTHER}/x?node-id=1-2` },
    ];
    for (const args of refused) {
      const reply = await call("figmog_node", { id: "1:2", ...args });
      assert.equal(reply.status, 403, `${JSON.stringify(args)}: ${reply.body}`);
    }
    assert.deepEqual(await forwarded(), before);
    const library = await call("figmog_node", { id: `https://www.figma.com/design/FakeLibraryFile01/x?node-id=1-2` });
    assert.equal(library.status, 200, library.body);
    assert.equal(fileKey(`https://www.figma.com/board/${OTHER}/x`), OTHER);
    assert.equal(fileKey("https://www.figma.com/proto/x"), null);
  });

  test("rejects malformed requests", async () => {
    const cases: [Parameters<typeof send>[1], number][] = [
      [{ method: "GET", headers: page() }, 405],
      [{ body: "{}", headers: page({ "content-type": "text/plain" }) }, 415],
      [{ body: "not json", headers: page() }, 400],
      [{ body: '{"tool": 1}', headers: page() }, 400],
      [{ body: '{"tool": "figmog_status", "arguments": []}', headers: page() }, 400],
      [{ body: JSON.stringify({ tool: "figmog_status", arguments: { q: "x".repeat(70_000) } }), headers: page() }, 413],
      [{ path: "/__figmog/other", body: "{}", headers: page() }, 404],
    ];
    for (const [options, status] of cases) {
      const reply = await send(port, options);
      assert.equal(reply.status, status, `${JSON.stringify(options).slice(0, 120)}: ${reply.body}`);
    }
    // Paths outside /__figmog/ are Vite's.
    assert.equal((await send(port, { method: "GET", path: "/index.html", headers: { host: `localhost:${port}` } })).status, 200);
  });

  test("reports figmog going away and starts it again on the next call", async () => {
    const crash = await call("figmog_status", { exit: true });
    assert.equal(crash.status, 502, crash.body);
    assert.match(crash.body, /figmog exited/);
    const again = await call("figmog_status");
    assert.equal(again.status, 200, again.body);
  });
});
