// The figmog Vite plugin (NWS-2957). Development server only (`apply:
// "serve"`): a production build never runs it, so built output contains
// nothing from figmog — no attributes, no bridge, no token.
import { randomBytes } from "node:crypto";
import { realpathSync } from "node:fs";
import { resolve } from "node:path";
import type { TLSSocket } from "node:tls";
import type { Logger, Plugin, ResolvedConfig } from "vite";
import { BRIDGE_GLOBAL, BRIDGE_PATH, handleBridgeRequest, isBridgeUrl, protectHtmlResponse, removeCorsHeaders } from "./bridge.js";
import { figmogBinary, message, readMapTargets, readSettings, type ProjectSettings } from "./figmog.js";
import { markModule, type MarkSpec } from "./mark.js";
import { FigmogRelay } from "./relay.js";

export interface FigmogPluginOptions {
  /** The figmog binary. Default: `FIGMOG_BIN`, else `figmog` on `PATH`. */
  figmog?: string;
  /**
   * Automatic `data-figmog` / `data-figmog-node` attributes on the root
   * element of each component in figmog.map.json. Default: figmog.toml's
   * `[inspector] attributes` (off unless set). Explicit locators (a
   * `data-testid` in figmog.targets.json) come first; these are optional.
   */
  attributes?: boolean;
  /** Serve the local bridge at `/__figmog/bridge`. Default `true`. */
  bridge?: boolean;
  /**
   * Extra arguments for the `figmog serve` relay the bridge starts on first
   * use (for example `["--no-watch"]`). Default: none — the relay starts the
   * repository's background figmog with the same defaults an agent's
   * `figmog serve` would.
   */
  serveArgs?: readonly string[];
}

function canonical(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return resolve(path);
  }
}

/** Code file → the exports to mark in it. */
async function markPlan(
  bin: string,
  settings: ProjectSettings,
  logger: Logger,
): Promise<Map<string, MarkSpec[]>> {
  const plan = new Map<string, MarkSpec[]>();
  const { targets, warnings } = await readMapTargets(bin, settings.projectRoot);
  for (const warning of warnings) logger.warn(`figmog: ${warning}`);
  // One export mapped from two Figma components cannot say which one it is.
  const canonicalTargets = targets.map((target) => ({
    ...target,
    path: canonical(resolve(settings.projectRoot, target.path)),
  }));
  const seen = new Map<string, number>();
  for (const target of canonicalTargets) {
    const id = `${target.path}#${target.export}`;
    seen.set(id, (seen.get(id) ?? 0) + 1);
  }
  for (const target of canonicalTargets) {
    const id = `${target.path}#${target.export}`;
    if ((seen.get(id) ?? 0) > 1) {
      logger.warn(`figmog: not marking ${id}: several figmog.map.json entries name it; mark it explicitly`);
      continue;
    }
    const file = target.path;
    const specs = plan.get(file) ?? [];
    specs.push({
      exportName: target.export,
      figma: `${target.kind}:${target.key}`,
      node: target.nodeIds.length === 1 ? (target.nodeIds[0] ?? null) : null,
    });
    plan.set(file, specs);
  }
  return plan;
}

/** The Vite plugin: dev-only attributes and the local figmog bridge. */
export function figmog(options: FigmogPluginOptions = {}): Plugin {
  const bin = figmogBinary(options.figmog);
  let logger: Logger | null = null;
  let settings: ProjectSettings | null = null;
  let plan = new Map<string, MarkSpec[]>();
  let token: string | null = null;
  let relay: FigmogRelay | null = null;
  // Inline plugin objects survive server.restart(). Each container closes
  // only the relay created for its own resolved configuration.
  const relays = new WeakMap<ResolvedConfig, FigmogRelay>();
  const warned = new Set<string>();

  return {
    name: "figmog",
    apply: "serve",
    enforce: "pre",

    async configResolved(config: ResolvedConfig) {
      logger = config.logger;
      settings = null;
      plan = new Map();
      token = null;
      relay = null;
      warned.clear();
      try {
        settings = await readSettings(bin, config.root);
      } catch (error) {
        config.logger.warn(`figmog: plugin disabled (no attributes, no bridge): ${message(error)}`);
        return;
      }
      if (options.attributes ?? settings.attributes) {
        try {
          plan = await markPlan(bin, settings, config.logger);
        } catch (error) {
          config.logger.warn(`figmog: automatic attributes disabled: ${message(error)}`);
        }
      }
      if (options.bridge ?? true) {
        token = randomBytes(32).toString("base64url");
        relay = new FigmogRelay({ bin, cwd: settings.projectRoot, args: options.serveArgs ?? [] });
        relays.set(config, relay);
      }
    },

    configureServer(server) {
      if (settings === null || token === null || relay === null) return;
      const config = {
        token,
        files: [...new Set(Object.values(settings.files))].sort(),
        ...(settings.files["default"] === undefined ? {} : { defaultFile: settings.files["default"] }),
        call: relay.call.bind(relay),
      };
      server.middlewares.use((request, response, next) => {
        if (!isBridgeUrl(request.url)) {
          // HTML carries a session credential. Refuse other origins before
          // Vite renders it, removing any CORS permission already added.
          const scheme = (request.socket as TLSSocket).encrypted === true ? "https" : "http";
          const origin = request.headers.origin;
          if (origin !== undefined && origin !== `${scheme}://${request.headers.host}`) {
            removeCorsHeaders(response);
            response.writeHead(403, { "cache-control": "no-store", "content-type": "text/plain; charset=utf-8" });
            response.end("the figmog dev server only answers its own origin");
            return;
          }
          protectHtmlResponse(response);
          next();
          return;
        }
        void handleBridgeRequest(request, response, config);
      });
      const stop = relay;
      server.httpServer?.once("close", () => {
        void stop.close();
      });
    },

    transformIndexHtml() {
      if (token === null) return undefined;
      const info = JSON.stringify({ token, url: BRIDGE_PATH }).replace(/</g, "\\u003c");
      return [
        {
          tag: "script",
          children: `window.${BRIDGE_GLOBAL}=Object.freeze(${info});`,
          injectTo: "head-prepend",
        },
      ];
    },

    transform(code, id) {
      if (plan.size === 0 || id.startsWith("\0")) return null;
      const file = id.split("?")[0] ?? id;
      const specs = plan.get(canonical(file));
      if (specs === undefined) return null;
      const outcome = markModule(code, file, specs);
      for (const { exportName, reason } of outcome.skipped) {
        const text = `figmog: not marking ${exportName} (${file}): ${reason}`;
        if (!warned.has(text)) {
          warned.add(text);
          logger?.warn(text);
        }
      }
      return outcome.code === null ? null : { code: outcome.code, map: null };
    },

    async closeBundle() {
      const config = this.environment.getTopLevelConfig();
      const owned = relays.get(config);
      relays.delete(config);
      await owned?.close();
    },
  };
}

export default figmog;
