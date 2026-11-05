// Verification targets (NWS-2957): the screenshot runner driven by a target
// manifest, for figmog's verification loop (NWS-2951). Each target names the
// Figma node it implements, a route on the dev server and an explicit
// locator; a capture fails unless the locator matches exactly one element.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { captureToDir, type CaptureManifest, type Viewport } from "./capture.js";
import { stableJson } from "./digest.js";

/** The manifest's file name at the project root. */
export const TARGETS_FILE = "figmog.targets.json";

/** An explicit locator: a `data-testid` value, or a CSS selector. */
export type Locator = { testid: string } | { css: string };

export interface Target {
  /** Lowercase letters, digits and `-`; used in output file names. */
  name: string;
  /** The Figma node the target implements. */
  figma: { node: string };
  /** Path on the dev server, starting with `/` (query allowed). */
  route: string;
  locator: Locator;
}

/**
 * The v1 verify contract's viewports (`figmog settings`: widths 1440 and 390,
 * light, default state), with the heights the slice uses.
 */
export const CONTRACT_VIEWPORTS: readonly Viewport[] = [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
];

const NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;

function fail(file: string, path: string, message: string): never {
  throw new Error(`${file}: ${path}: ${message}`);
}

function object(value: unknown, file: string, path: string, keys: readonly string[]): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) fail(file, path, "expected an object");
  const record = value as Record<string, unknown>;
  for (const key of Object.keys(record).sort()) {
    if (!keys.includes(key)) fail(file, `${path}.${key}`, "unknown key");
  }
  return record;
}

function text(value: unknown, file: string, path: string): string {
  if (typeof value !== "string" || value === "") fail(file, path, "expected a non-empty string");
  return value;
}

/**
 * Parses and validates a target manifest:
 * `{"version": 1, "targets": [{name, figma: {node}, route, locator}]}`.
 * Unknown keys, a duplicate name, a route that is not a path and a locator
 * that is not exactly one of `testid` / `css` are errors naming the JSON path.
 */
export function parseTargets(source: string, file = TARGETS_FILE): Target[] {
  let raw: unknown;
  try {
    raw = JSON.parse(source) as unknown;
  } catch (error) {
    fail(file, "$", `not JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  const top = object(raw, file, "$", ["targets", "version"]);
  if (top["version"] !== 1) fail(file, "version", "expected 1");
  if (!Array.isArray(top["targets"])) fail(file, "targets", "expected an array");
  const names = new Set<string>();
  return top["targets"].map((entry: unknown, index: number): Target => {
    const at = `targets[${index}]`;
    const target = object(entry, file, at, ["figma", "locator", "name", "route"]);
    const name = text(target["name"], file, `${at}.name`);
    if (!NAME.test(name)) fail(file, `${at}.name`, `${JSON.stringify(name)} is not lowercase letters, digits and -`);
    if (names.has(name)) fail(file, `${at}.name`, `duplicate target ${JSON.stringify(name)}`);
    names.add(name);
    const figma = object(target["figma"], file, `${at}.figma`, ["node"]);
    const route = text(target["route"], file, `${at}.route`);
    if (!route.startsWith("/") || route.startsWith("//")) {
      fail(file, `${at}.route`, "expected a path on the dev server starting with /");
    }
    const locator = object(target["locator"], file, `${at}.locator`, ["css", "testid"]);
    const kinds = Object.keys(locator);
    if (kinds.length !== 1) fail(file, `${at}.locator`, 'expected exactly one of "testid" or "css"');
    const parsedLocator: Locator =
      "testid" in locator
        ? { testid: text(locator["testid"], file, `${at}.locator.testid`) }
        : { css: text(locator["css"], file, `${at}.locator.css`) };
    return { name, figma: { node: text(figma["node"], file, `${at}.figma.node`) }, route, locator: parsedLocator };
  });
}

/** A CSS string literal (double-quoted) for `value`. */
function cssString(value: string): string {
  return `"${value.replace(/[\\"]/g, (c) => `\\${c}`).replace(/[\n\r\f]/g, (c) => `\\${c.charCodeAt(0).toString(16)} `)}"`;
}

/** The CSS selector a locator stands for. */
export function locatorSelector(locator: Locator): string {
  return "testid" in locator ? `[data-testid=${cssString(locator.testid)}]` : locator.css;
}

export interface TargetCaptureOptions {
  /** Path of the target manifest. */
  manifest: string;
  /** The target's name. */
  target: string;
  /** The dev server's URL (figmog.toml `[verify] dev_server`). */
  baseUrl: string;
  outDir: string;
  /** Default: {@link CONTRACT_VIEWPORTS}. */
  viewports?: readonly Viewport[];
  executablePath?: string;
}

export interface TargetCapture {
  viewport: Viewport;
  pngPath: string;
  manifestPath: string;
  manifest: CaptureManifest;
}

export interface TargetResult {
  target: Target;
  url: string;
  selector: string;
  captures: TargetCapture[];
  /** `<outDir>/<target>.target.json`: the target and every capture. */
  summaryPath: string;
}

/**
 * Captures one target at each viewport: `<outDir>/<target>-<W>x<H>.png` and
 * its `.manifest.json`, plus `<outDir>/<target>.target.json`. Fails (a
 * `SelectorMatchError`) when the locator matches no element or several.
 */
export async function captureTarget(options: TargetCaptureOptions): Promise<TargetResult> {
  const targets = parseTargets(await readFile(options.manifest, "utf8"), options.manifest);
  const target = targets.find((t) => t.name === options.target);
  if (target === undefined) {
    const known = targets.map((t) => t.name).sort().join(", ");
    throw new Error(`${options.manifest}: no target ${JSON.stringify(options.target)} (targets: ${known || "none"})`);
  }
  const url = new URL(target.route, options.baseUrl).href;
  const selector = locatorSelector(target.locator);
  const captures: TargetCapture[] = [];
  for (const viewport of options.viewports ?? CONTRACT_VIEWPORTS) {
    const { manifest, manifestPath, pngPath } = await captureToDir({
      name: `${target.name}-${viewport.width}x${viewport.height}`,
      outDir: options.outDir,
      selector,
      url,
      viewport,
      ...(options.executablePath === undefined ? {} : { executablePath: options.executablePath }),
    });
    captures.push({ manifest, manifestPath, pngPath, viewport });
  }
  await mkdir(options.outDir, { recursive: true });
  const summaryPath = join(options.outDir, `${target.name}.target.json`);
  await writeFile(
    summaryPath,
    stableJson({
      captures: captures.map((c) => ({
        image: c.manifest.image,
        manifest: `${c.manifest.image.file.replace(/\.png$/, "")}.manifest.json`,
        viewport: c.viewport,
      })),
      kind: "figmog-shot/target",
      selector,
      target,
      url,
      version: 1,
    }),
  );
  return { captures, selector, summaryPath, target, url };
}
