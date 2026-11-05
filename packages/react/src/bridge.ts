// The local bridge (NWS-2957): browser code in the dev page asks the
// project's figmog for node data, tokens and renders through one endpoint of
// the dev server. The dev server is not a security boundary on its own
// (`vite --host` exposes it to the network), so every request must:
//
// - come from this machine (loopback peer address);
// - name a loopback Host (`localhost`, `127.0.0.1` or `[::1]`), which also
//   defeats DNS rebinding;
// - come from the dev page itself: `Origin` equal to the request's own
//   origin, so another local dev server's page (another project) is refused;
// - carry the per-session token the dev server injected into its page;
// - call an allowlisted read tool or `figmog_images` (renders), never a
//   pull, management, internal or desktop-proxy tool;
// - name only this project's Figma files (figmog.toml `[files]`).
//
// The Figma token never leaves figmog: the bridge forwards tool calls and
// returns their results, nothing else.
import { timingSafeEqual } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { TLSSocket } from "node:tls";
import { isObject } from "./figmog.js";

/** The bridge's one endpoint, on the dev server. */
export const BRIDGE_PATH = "/__figmog/bridge";
/** The request header carrying the session token. */
export const TOKEN_HEADER = "x-figmog-token";
/** The global the dev page gets: `{ url, token }`. */
export const BRIDGE_GLOBAL = "__FIGMOG_BRIDGE__";

/**
 * Local reads of the mirror (spec §6: zero Figma API cost). Not
 * `figmog_files` (lists every mirror of the repository, not one project's
 * files), `figmog_open` / `figmog_sync` (pulls), `figmog_internal_*` (CLI
 * writes) or any desktop-proxy tool.
 */
export const READ_TOOLS: readonly string[] = [
  "figmog_at",
  "figmog_components",
  "figmog_find",
  "figmog_instances",
  "figmog_node",
  "figmog_pages",
  "figmog_path",
  "figmog_search",
  "figmog_spec",
  "figmog_stats",
  "figmog_status",
  "figmog_styles",
  "figmog_subtree",
  "figmog_text",
  "figmog_tree",
  "figmog_uses",
  "figmog_vars",
  "figmog_where",
];
/** Renders and image fills (spends Figma's images budget, cached by figmog). */
export const RENDER_TOOLS: readonly string[] = ["figmog_images"];
export const ALLOWED_TOOLS: ReadonlySet<string> = new Set([...READ_TOOLS, ...RENDER_TOOLS]);

/** Request bodies above this are refused. */
export const MAX_BODY = 64 * 1024;
/** How long a read may take; renders get {@link RENDER_TIMEOUT_MS}. */
export const READ_TIMEOUT_MS = 30_000;
export const RENDER_TIMEOUT_MS = 150_000;

export interface BridgeConfig {
  /** The per-session token (random, generated at dev-server start). */
  token: string;
  /** This project's Figma file keys (figmog.toml `[files]`). */
  files: readonly string[];
  /** Forwards an allowed call to figmog; resolves with its MCP result. */
  call: (tool: string, args: Record<string, unknown>, timeoutMs: number) => Promise<unknown>;
}

/** A refused or failed bridge request. */
export class BridgeError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "BridgeError";
    this.status = status;
  }
}

const LOOPBACK_ADDRESSES = new Set(["127.0.0.1", "::1", "::ffff:127.0.0.1"]);
const LOOPBACK_HOST = /^(localhost|127\.0\.0\.1|\[::1\])(:[0-9]{1,5})?$/;

/** Whether a socket peer address is this machine's loopback. */
export function isLoopbackAddress(address: string | undefined): boolean {
  return address !== undefined && LOOPBACK_ADDRESSES.has(address);
}

/** Whether a Host header names a loopback host (`localhost`, `127.0.0.1`, `[::1]`). */
export function isLoopbackHost(host: string | undefined): boolean {
  return host !== undefined && LOOPBACK_HOST.test(host);
}

const FILE_KEY = /^[A-Za-z0-9]{10,}$/;

/**
 * The Figma file key a `file` argument or node URL names (figmog's
 * `parse_file_ref`: a bare key, or a `figma.com/design|file|board/<key>` URL).
 */
export function fileKey(input: string): string | null {
  if (FILE_KEY.test(input)) return input;
  const at = input.indexOf("figma.com/");
  if (at < 0) return null;
  const [kind, key] = input.slice(at + "figma.com/".length).split("/");
  if (kind !== "design" && kind !== "file" && kind !== "board") return null;
  return key !== undefined && FILE_KEY.test(key) ? key : null;
}

/** Node-id arguments that may carry a Figma URL naming a file. */
const NODE_REF_ARGS = ["id", "target", "under"] as const;

/**
 * Checks a parsed call against the allowlist and this project's files.
 * Throws a {@link BridgeError} (403) when it is refused.
 */
export function checkCall(tool: string, args: Record<string, unknown>, files: readonly string[]): void {
  if (!ALLOWED_TOOLS.has(tool)) {
    throw new BridgeError(403, `${JSON.stringify(tool)} is not available through the bridge (read tools and renders only)`);
  }
  const allowed = new Set(files);
  const file = args["file"];
  if (file !== undefined && file !== null) {
    const key = typeof file === "string" ? fileKey(file) : null;
    if (key === null || !allowed.has(key)) {
      throw new BridgeError(403, "file is not one of this project's Figma files (figmog.toml [files])");
    }
  }
  for (const name of NODE_REF_ARGS) {
    const value = args[name];
    if (typeof value === "string" && value.includes("figma.com/")) {
      const key = fileKey(value);
      if (key !== null && !allowed.has(key)) {
        throw new BridgeError(403, `${name} names a Figma file that is not one of this project's files`);
      }
    }
  }
  if (tool === "figmog_images" && args["refresh"] === true) {
    throw new BridgeError(403, "refresh is not available through the bridge; use `figmog images --refresh`");
  }
}

function tokenMatches(expected: string, given: string | string[] | undefined): boolean {
  if (typeof given !== "string") return false;
  const a = Buffer.from(expected);
  const b = Buffer.from(given);
  return a.length === b.length && timingSafeEqual(a, b);
}

/**
 * Checks where a request comes from: loopback peer, loopback Host, same-origin
 * `Origin`, the session token. Throws a {@link BridgeError} (403 or 401).
 */
export function checkRequest(request: IncomingMessage, token: string): void {
  if (!isLoopbackAddress(request.socket.remoteAddress)) {
    throw new BridgeError(403, "the figmog bridge only answers requests from this machine");
  }
  const host = request.headers.host;
  if (!isLoopbackHost(host)) {
    throw new BridgeError(403, "the figmog bridge only answers on localhost, 127.0.0.1 or [::1]");
  }
  const scheme = (request.socket as TLSSocket).encrypted === true ? "https" : "http";
  if (request.headers.origin !== `${scheme}://${host}`) {
    throw new BridgeError(403, "the figmog bridge only answers the dev page's own origin");
  }
  const site = request.headers["sec-fetch-site"];
  if (site !== undefined && site !== "same-origin") {
    throw new BridgeError(403, "the figmog bridge only answers same-origin requests");
  }
  if (!tokenMatches(token, request.headers[TOKEN_HEADER])) {
    throw new BridgeError(401, "missing or wrong figmog bridge token");
  }
}

function readBody(request: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    let tooLarge = false;
    request.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > MAX_BODY) tooLarge = true;
      else chunks.push(chunk);
    });
    request.on("end", () => {
      if (tooLarge) reject(new BridgeError(413, `request body over ${MAX_BODY} bytes`));
      else resolve(Buffer.concat(chunks).toString("utf8"));
    });
    request.on("error", reject);
  });
}

/**
 * CORS headers Vite's own `server.cors` middleware (which runs before plugin
 * middlewares and allows every localhost origin by default) may have set:
 * the bridge answers its own page only, so no other origin may read it.
 */
const CORS_HEADERS = [
  "access-control-allow-credentials",
  "access-control-allow-headers",
  "access-control-allow-methods",
  "access-control-allow-origin",
  "access-control-expose-headers",
];

function send(response: ServerResponse, status: number, body: unknown): void {
  const text = JSON.stringify(body);
  for (const header of CORS_HEADERS) response.removeHeader(header);
  response.writeHead(status, {
    "cache-control": "no-store",
    "content-length": Buffer.byteLength(text),
    "content-type": "application/json; charset=utf-8",
    "x-content-type-options": "nosniff",
  });
  response.end(text);
}

/**
 * Handles one bridge request: `POST {BRIDGE_PATH}` with a JSON body
 * `{"tool": "<name>", "arguments": {...}}`, answered with the tool's MCP
 * `CallToolResult` (200) or `{"error": "..."}` (4xx/5xx).
 */
export async function handleBridgeRequest(
  request: IncomingMessage,
  response: ServerResponse,
  config: BridgeConfig,
): Promise<void> {
  try {
    checkRequest(request, config.token);
    if (request.url?.split("?")[0] !== BRIDGE_PATH) throw new BridgeError(404, "no such figmog bridge endpoint");
    if (request.method !== "POST") throw new BridgeError(405, "use POST");
    const type = request.headers["content-type"] ?? "";
    if (!/^application\/json(;|$)/i.test(type)) throw new BridgeError(415, "send application/json");
    let body: unknown;
    try {
      body = JSON.parse(await readBody(request)) as unknown;
    } catch (error) {
      if (error instanceof BridgeError) throw error;
      throw new BridgeError(400, "the request body is not JSON");
    }
    if (!isObject(body) || typeof body["tool"] !== "string") {
      throw new BridgeError(400, 'expected {"tool": "<name>", "arguments": {...}}');
    }
    const args = body["arguments"] ?? {};
    if (!isObject(args)) throw new BridgeError(400, "arguments must be an object");
    checkCall(body["tool"], args, config.files);
    const timeout = RENDER_TOOLS.includes(body["tool"]) ? RENDER_TIMEOUT_MS : READ_TIMEOUT_MS;
    send(response, 200, await config.call(body["tool"], args, timeout));
  } catch (error) {
    const status = isObject(error) && typeof error["status"] === "number" ? error["status"] : 500;
    send(response, status, { error: error instanceof Error ? error.message : String(error) });
  }
}

/** Whether a request URL is under `/__figmog/`, which the bridge owns. */
export function isBridgeUrl(url: string | undefined): boolean {
  return url !== undefined && url.startsWith("/__figmog/");
}
