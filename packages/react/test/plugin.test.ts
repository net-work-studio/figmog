// The Vite plugin and the bridge's security rules (NWS-2957), against a real
// Vite dev server and a stand-in figmog binary (test/fixtures/fake-figmog.mjs),
// so no Rust build is needed. The end-to-end run against the real figmog
// binary is examples/dev-plugin's test.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import type { AddressInfo } from "node:net";
import { networkInterfaces, tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { chromium } from "playwright";
import { build, createLogger, createServer, type InlineConfig, type ViteDevServer } from "vite";
import { BRIDGE_PATH, fileKey, isLoopbackAddress, isLoopbackHost } from "../src/bridge.js";
import { figmog, type FigmogPluginOptions } from "../src/vite.js";
import { FIXTURES, serveFixtures } from "./server.js";

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

async function bridgeToken(devPort: number): Promise<string> {
  const html = await (await fetch(`http://localhost:${devPort}/`)).text();
  const match = /window\.__FIGMOG_BRIDGE__=Object\.freeze\((\{.*?\})\);/.exec(html);
  assert.ok(match?.[1], "the dev page carries the bridge token");
  return (JSON.parse(match[1]) as { token: string }).token;
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

  test("equivalent map paths cannot mark the same export twice", async () => {
    const root = join(work, "duplicate-paths");
    await mkdir(join(root, "src"), { recursive: true });
    await writeFile(join(root, "src", "Card.jsx"), "export const Card = () => <div/>;");
    await writeFile(join(root, "figmog.map.json"), JSON.stringify({
      components: ["src/Card.jsx", "src/./Card.jsx"].map((path, index) => ({
        figma: { component: `synthetic-component-${index}` },
        code: { path, export: "Card" },
      })),
    }));
    const off = await createServer(config({ attributes: true, bridge: false }, { root, server: { middlewareMode: true } }));
    try {
      const card = await off.transformRequest("/src/Card.jsx");
      assert.doesNotMatch(card?.code ?? "", /data-figmog/);
      assert.ok(warnings.some((warning) => /duplicate-paths.*Card.jsx#Card.*several/.test(warning)));
    } finally {
      await off.close();
    }
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

  test("pins implicit calls to the declared default instead of the relay's current file", async () => {
    const reply = await call("figmog_node", { id: "1:2" });
    assert.equal(reply.status, 200, reply.body);
    const result = JSON.parse(reply.body) as { content: { text: string }[] };
    assert.deepEqual(JSON.parse(result.content[0]?.text ?? ""), {
      arguments: { file: PRODUCT, id: "1:2" },
      tool: "figmog_node",
    });
  });

  test("refuses stale implicit defaults for library-only and empty file allowlists", async () => {
    for (const files of [{ library: "FakeLibraryFile01" }, {}]) {
      const root = await mkdtemp(join(work, "stale-current-"));
      await mkdir(join(root, ".figmog"));
      await writeFile(join(root, ".figmog", "current"), OTHER);
      await writeFile(join(root, "index.html"), "<!doctype html><title>synthetic stale-current fixture</title>");
      const fake = join(root, "fake-figmog.mjs");
      const source = (await readFile(FAKE, "utf8")).replace(
        'const FILES = { default: "FakeProductFile01", library: "FakeLibraryFile01" };',
        `const FILES = ${JSON.stringify(files)};`,
      );
      await writeFile(fake, source, { mode: 0o755 });
      const isolated = await createServer(config({ figmog: fake, attributes: false }, {
        root,
        server: { host: "127.0.0.1", port: 0 },
      }));
      try {
        await isolated.listen();
        const isolatedPort = (isolated.httpServer?.address() as AddressInfo).port;
        const isolatedToken = await bridgeToken(isolatedPort);
        const before = await forwarded();
        for (const tool of ["figmog_node", "figmog_images", "figmog_status"]) {
          const reply = await send(isolatedPort, {
            body: JSON.stringify({ tool, arguments: { id: "1:2" } }),
            headers: page({
              host: `localhost:${isolatedPort}`,
              origin: `http://localhost:${isolatedPort}`,
              "x-figmog-token": isolatedToken,
            }),
          });
          assert.equal(reply.status, 403, reply.body);
          assert.match(reply.body, /no allowlisted default/);
        }
        assert.deepEqual(await forwarded(), before, "stale defaults never reached the relay");
        if ("library" in files) {
          const reply = await send(isolatedPort, {
            body: JSON.stringify({ tool: "figmog_node", arguments: { id: "1:2", file: files.library } }),
            headers: page({
              host: `localhost:${isolatedPort}`,
              origin: `http://localhost:${isolatedPort}`,
              "x-figmog-token": isolatedToken,
            }),
          });
          assert.equal(reply.status, 200, reply.body);
        }
      } finally {
        await isolated.close();
      }
    }
  });

  test("an inline plugin's bridge survives server.restart()", async () => {
    const restarting = await createServer(config({ attributes: false }, { server: { host: "127.0.0.1", port: 0 } }));
    try {
      await restarting.listen();
      let previousToken = "";
      for (let generation = 0; generation < 3; generation++) {
        const restartPort = (restarting.httpServer?.address() as AddressInfo).port;
        const nextToken = await bridgeToken(restartPort);
        assert.notEqual(nextToken, previousToken, "each generation gets its own credential");
        previousToken = nextToken;
        const reply = await send(restartPort, {
          body: JSON.stringify({ tool: "figmog_node", arguments: { id: "1:2" } }),
          headers: page({
            host: `localhost:${restartPort}`,
            origin: `http://localhost:${restartPort}`,
            "x-figmog-token": nextToken,
          }),
        });
        assert.equal(reply.status, 200, `generation ${generation}: ${reply.body}`);
        if (generation < 2) await restarting.restart();
      }
    } finally {
      await restarting.close();
    }
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

  test("never exposes token-bearing HTML to another localhost origin", async (t) => {
    const reply = await fetch(`http://localhost:${port}/`, { headers: { origin: `http://localhost:${port + 1}` } });
    assert.equal(reply.status, 403);
    assert.equal(reply.headers.get("access-control-allow-origin"), null);
    assert.doesNotMatch(await reply.text(), /__FIGMOG_BRIDGE__|[A-Za-z0-9_-]{43}/);
    const same = await fetch(`http://localhost:${port}/`, { headers: { origin: `http://localhost:${port}` } });
    assert.equal(same.status, 200);
    assert.match(await same.text(), /__FIGMOG_BRIDGE__/);

    const other = await serveFixtures();
    t.after(other.close);
    const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
    const browser = await chromium.launch({
      headless: true,
      ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
    });
    try {
      const tab = await browser.newPage();
      await tab.goto(`${other.origin}/card.html`);
      const readable = await tab.evaluate(async (url) => {
        try {
          return (await (await fetch(url)).text()).includes("__FIGMOG_BRIDGE__");
        } catch {
          return false;
        }
      }, `http://127.0.0.1:${port}/`);
      assert.equal(readable, false, "another localhost page cannot read the injected credential");
    } finally {
      await browser.close();
    }
  });

  test("a browser startup failure releases the fixture server and exits", async () => {
    const missing = join(work, "missing-chromium");
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      FIGMOG_SHOT_CHROMIUM: missing,
      PLAYWRIGHT_BROWSERS_PATH: join(work, "no-playwright-browsers"),
    };
    // A child run needs readable TAP, not the parent worker's binary IPC reporter.
    delete env["NODE_TEST_CONTEXT"];
    await assert.rejects(
      // Run in one process so the timeout also bounds any leaked HTTP server.
      promisify(execFile)(process.execPath, [
        "--test-name-pattern=never exposes token-bearing HTML",
        fileURLToPath(import.meta.url),
      ], {
        env,
        killSignal: "SIGKILL",
        timeout: 15_000,
      }),
      (error: unknown) => {
        const failure = error as Error & {
          code?: number; killed?: boolean; signal?: string | null; stdout?: string; stderr?: string;
        };
        const output = `${failure.stdout ?? ""}${failure.stderr ?? ""}`;
        assert.equal(failure.killed, false, `the failed launch must exit without leaking its fixture server: ${output}`);
        assert.equal(failure.code, 1, output);
        assert.equal(failure.signal, null, output);
        assert.match(output, /executable.*doesn't exist/i);
        assert.ok(output.includes(missing), output);
        return true;
      },
    );
  });

  test("cached dev HTML cannot expose its token to another localhost origin", async (t) => {
    const root = join(work, "cache-html");
    await mkdir(root);
    await writeFile(join(root, "index.html"), "<!doctype html><title>Synthetic cache fixture</title>");
    const isolated = await createServer(config({ attributes: false }, {
      root,
      server: { cors: true, host: "127.0.0.1", port: 0 },
    }));
    t.after(() => isolated.close());
    await isolated.listen();
    const isolatedPort = (isolated.httpServer?.address() as AddressInfo).port;
    const origin = `http://127.0.0.1:${isolatedPort}`;
    const other = await serveFixtures();
    t.after(other.close);
    const executablePath = process.env["FIGMOG_SHOT_CHROMIUM"];
    const browser = await chromium.launch({
      headless: true,
      ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
    });
    try {
      const tab = await browser.newPage();
      await tab.goto(`${origin}/`);
      const seeded = await tab.evaluate(async () => {
        const response = await fetch("/");
        return {
          body: await response.text(),
          cacheControl: response.headers.get("cache-control"),
          cors: response.headers.get("access-control-allow-origin"),
          status: response.status,
        };
      });
      assert.equal(seeded.status, 200);
      assert.match(seeded.body, /window\.__FIGMOG_BRIDGE__/);
      const pageToken = /"token":"([A-Za-z0-9_-]{43})"/.exec(seeded.body)?.[1];
      assert.ok(pageToken);
      await tab.goto(`${other.origin}/card.html`);
      const leaked = await tab.evaluate(async ({ url, token }) => {
        try {
          const response = await fetch(url, { cache: "force-cache" });
          return { exposesToken: (await response.text()).includes(token), status: response.status };
        } catch {
          return null;
        }
      }, { token: pageToken, url: `${origin}/` });
      assert.equal(leaked, null, "another localhost origin must not read cached token-bearing HTML");
      assert.equal(seeded.cors, null);
      assert.equal(seeded.cacheControl, "no-store");
    } finally {
      await browser.close();
    }
  });

  test("HTML protection survives later middleware headers without changing other responses", async () => {
    for (const bridge of [true, false]) {
      const base = config({ attributes: false, bridge }, { server: { cors: true, host: "127.0.0.1", port: 0 } });
      const isolated = await createServer({
        ...base,
        plugins: [...(base.plugins ?? []), {
          name: "synthetic-late-html",
          configureServer(dev: ViteDevServer) {
            dev.middlewares.use((request, response, next) => {
              const style = request.url ?? "";
              if (!["/implicit", "/object", "/array", "/no-reason", "/plain"].includes(style)) return next();
              const headers = {
                "Access-Control-Allow-Origin": "*",
                "Cache-Control": "public, max-age=600",
                "Content-Type": style === "/plain" ? "text/plain" : "text/html; charset=utf-8",
                "Set-Cookie": ["first=1", "second=2"],
              };
              if (style === "/plain") {
                response.writeHead(200, headers).end("Synthetic plain fixture");
                return;
              }
              void dev.transformIndexHtml("/index.html", "<!doctype html><title>Synthetic headers fixture</title>").then(
                (html) => {
                  if (style === "/implicit") {
                    for (const [name, value] of Object.entries(headers)) response.setHeader(name, value);
                  } else if (style === "/array") {
                    response.writeHead(200, [
                      "Content-Type", headers["Content-Type"], "Set-Cookie", "first=1", "Set-Cookie", "second=2",
                      "ACCESS-CONTROL-ALLOW-ORIGIN", "*", "cAcHe-CoNtRoL", headers["Cache-Control"],
                    ]);
                  } else if (style === "/no-reason") {
                    response.writeHead(200, undefined, headers);
                  } else {
                    response.writeHead(200, "Synthetic OK", headers);
                  }
                  response.end(html);
                },
              ).catch(next);
            });
          },
        }],
      });
      try {
        await isolated.listen();
        const isolatedPort = (isolated.httpServer?.address() as AddressInfo).port;
        for (const style of ["implicit", "object", "array", "no-reason", "plain"]) {
          const response = await fetch(`http://127.0.0.1:${isolatedPort}/${style}`);
          const body = await response.text();
          const privateHtml = bridge && style !== "plain";
          assert.equal(response.status, 200, body);
          assert.equal(response.headers.get("access-control-allow-origin"), privateHtml ? null : "*");
          assert.equal(response.headers.get("cache-control"), privateHtml ? "no-store" : "public, max-age=600");
          assert.deepEqual(response.headers.getSetCookie(), ["first=1", "second=2"]);
          assert.equal(body.includes("window.__FIGMOG_BRIDGE__"), privateHtml);
        }
      } finally {
        await isolated.close();
      }
    }
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
