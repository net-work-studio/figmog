import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

/**
 * The figmog binary the plugin and the bridge run: the `figmog` option, else
 * `FIGMOG_BIN`, else `figmog` on `PATH`.
 */
export function figmogBinary(option?: string): string {
  if (option !== undefined && option !== "") return option;
  const env = process.env["FIGMOG_BIN"];
  return env === undefined || env === "" ? "figmog" : env;
}

/** Runs `figmog ARGS` in `cwd` and parses its JSON stdout. */
export function figmogJson(bin: string, args: readonly string[], cwd: string): Promise<unknown> {
  return new Promise((resolve, reject) => {
    execFile(
      bin,
      [...args],
      { cwd, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, timeout: 120_000 },
      (error, stdout, stderr) => {
        if (error !== null) {
          reject(new Error(`\`figmog ${args.join(" ")}\` failed: ${errorText(stderr) ?? error.message}`));
          return;
        }
        try {
          resolve(JSON.parse(stdout) as unknown);
        } catch {
          reject(new Error(`\`figmog ${args.join(" ")}\` printed something that is not JSON`));
        }
      },
    );
  });
}

/** figmog prints failures as `{"error": "..."}` on stderr. */
function errorText(stderr: string): string | null {
  for (const line of stderr.split("\n").reverse()) {
    try {
      const parsed = JSON.parse(line) as unknown;
      if (isObject(parsed) && typeof parsed["error"] === "string") return parsed["error"];
    } catch {
      // not a JSON line
    }
  }
  const text = stderr.trim();
  return text === "" ? null : text;
}

export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The parts of `figmog settings` (NWS-2946) this package reads. */
export interface ProjectSettings {
  /** Absolute project root: where figmog.toml and figmog.map.json live. */
  projectRoot: string;
  /** `[files]` entries, name → file key. */
  files: Record<string, string>;
  /** `[inspector] attributes`: automatic `data-figmog` attributes. */
  attributes: boolean;
}

/**
 * Reads the project's settings through `figmog settings`, so figmog.toml and
 * figmog.local.toml are parsed by figmog itself, never here.
 */
export async function readSettings(bin: string, cwd: string): Promise<ProjectSettings> {
  const raw = await figmogJson(bin, ["settings"], cwd);
  if (!isObject(raw) || typeof raw["project_root"] !== "string" || !isObject(raw["files"])) {
    throw new Error("`figmog settings` printed an unexpected shape");
  }
  const files: Record<string, string> = {};
  for (const [name, key] of Object.entries(raw["files"])) {
    if (typeof key === "string") files[name] = key;
  }
  const inspector = isObject(raw["inspector"]) ? raw["inspector"] : {};
  return {
    projectRoot: raw["project_root"],
    files,
    attributes: inspector["attributes"] === true,
  };
}

/** One `figmog.map.json` entry, as the attribute plugin uses it. */
export interface MapTarget {
  kind: "component" | "component_set";
  key: string;
  /** Code file, relative to the project root. */
  path: string;
  export: string;
  /** The Figma node ids of the target in the mirror (empty when unknown). */
  nodeIds: string[];
}

export const MAP_FILE = "figmog.map.json";

/**
 * The project's map entries (NWS-2949). `figmog map check` validates the file
 * first (figmog's own rules); node ids come from `figmog map coverage` when
 * the mirror can answer, and are left empty (with a warning) otherwise.
 */
export async function readMapTargets(
  bin: string,
  projectRoot: string,
): Promise<{ targets: MapTarget[]; warnings: string[] }> {
  const path = join(projectRoot, MAP_FILE);
  if (!existsSync(path)) return { targets: [], warnings: [`no ${MAP_FILE} at ${projectRoot}: nothing to mark`] };
  await figmogJson(bin, ["map", "check"], projectRoot);
  const parsed = JSON.parse(await readFile(path, "utf8")) as { components: unknown[] };
  const targets: MapTarget[] = [];
  for (const entry of parsed.components) {
    if (!isObject(entry) || !isObject(entry["figma"]) || !isObject(entry["code"])) continue;
    const figma = entry["figma"];
    const kind = typeof figma["component_set"] === "string" ? "component_set" : "component";
    targets.push({
      kind,
      key: String(figma[kind]),
      path: String(entry["code"]["path"]),
      export: String(entry["code"]["export"]),
      nodeIds: [],
    });
  }
  const warnings: string[] = [];
  try {
    const coverage = await figmogJson(bin, ["map", "coverage"], projectRoot);
    const mapped = isObject(coverage) && Array.isArray(coverage["mapped"]) ? coverage["mapped"] : [];
    for (const row of mapped) {
      if (!isObject(row) || !isObject(row["figma"])) continue;
      const { kind, key, node_ids: nodeIds } = row["figma"];
      const target = targets.find((t) => t.kind === kind && t.key === key);
      if (target !== undefined && Array.isArray(nodeIds)) {
        target.nodeIds = nodeIds.filter((id): id is string => typeof id === "string").sort();
      }
    }
  } catch (error) {
    warnings.push(`Figma node ids unavailable, data-figmog-node is left out: ${message(error)}`);
  }
  return { targets, warnings };
}

export function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
