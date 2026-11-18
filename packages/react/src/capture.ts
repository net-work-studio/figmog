import { mkdir, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { join } from "node:path";
import { chromium } from "playwright";
import { sha256, stableJson } from "./digest.js";
import { decodePng } from "./png.js";

/** A CSS-pixel viewport. */
export interface Viewport {
  width: number;
  height: number;
}

/** Parses `WIDTHxHEIGHT`, e.g. `1440x900`. */
export function parseViewport(text: string): Viewport {
  const match = /^([1-9][0-9]{0,4})x([1-9][0-9]{0,4})$/.exec(text);
  if (match === null || match[1] === undefined || match[2] === undefined) {
    throw new Error(`invalid viewport ${JSON.stringify(text)}: expected WIDTHxHEIGHT, e.g. 1440x900`);
  }
  return { width: Number(match[1]), height: Number(match[2]) };
}

/**
 * Injected after load: stops CSS animations and transitions where they are
 * and hides the text caret, on top of `reducedMotion: "reduce"` and
 * Playwright's own `animations: "disabled"` screenshot option.
 */
export const DISABLE_MOTION_CSS = [
  "*, *::before, *::after {",
  "  animation-delay: 0s !important;",
  "  animation-duration: 0s !important;",
  "  animation-iteration-count: 1 !important;",
  "  animation-play-state: paused !important;",
  "  caret-color: transparent !important;",
  "  scroll-behavior: auto !important;",
  "  transition-delay: 0s !important;",
  "  transition-duration: 0s !important;",
  "}",
].join("\n");

/**
 * Chromium flags that remove machine-dependent rendering variation (colour
 * profile, LCD subpixel text, font hinting).
 */
export const CHROMIUM_ARGS: readonly string[] = [
  "--disable-lcd-text",
  "--font-render-hinting=none",
  "--force-color-profile=srgb",
];

/** The stable-environment settings each capture records in its manifest. */
export interface CaptureSettings {
  animations: "disabled";
  browser: { args: string[]; name: "chromium"; version: string };
  caret: "hidden";
  colorScheme: "light";
  deviceScaleFactor: 1;
  disableMotionCss: string;
  fontsReady: true;
  headless: true;
  locale: "en-US";
  playwright: string;
  reducedMotion: "reduce";
  timezoneId: "UTC";
}

export interface CaptureOptions {
  url: string;
  selector: string;
  viewport: Viewport;
  /**
   * Chromium binary to launch. Defaults to `FIGMOG_SHOT_CHROMIUM` when set,
   * otherwise the build Playwright resolves from its install location (or
   * `PLAYWRIGHT_BROWSERS_PATH`).
   */
  executablePath?: string;
}

export interface CaptureResult {
  png: Buffer;
  sha256: string;
  width: number;
  height: number;
  settings: CaptureSettings;
}

/** Thrown when the selector does not match exactly one element. */
export class SelectorMatchError extends Error {
  readonly count: number;
  constructor(selector: string, count: number) {
    super(
      count === 0
        ? `selector ${JSON.stringify(selector)} matched no element`
        : `selector ${JSON.stringify(selector)} is ambiguous: it matched ${count} elements, expected exactly 1`,
    );
    this.name = "SelectorMatchError";
    this.count = count;
  }
}

function playwrightVersion(): string {
  const require = createRequire(import.meta.url);
  const pkg = require("playwright/package.json") as { version: string };
  return pkg.version;
}

/**
 * Captures the single element matching `selector` on `url` in headless
 * Chromium under the stable environment described by {@link CaptureSettings}.
 */
export async function capture(options: CaptureOptions): Promise<CaptureResult> {
  const executablePath = options.executablePath ?? process.env["FIGMOG_SHOT_CHROMIUM"];
  const browser = await chromium.launch({
    args: [...CHROMIUM_ARGS],
    headless: true,
    ...(executablePath === undefined || executablePath === "" ? {} : { executablePath }),
  });
  try {
    const context = await browser.newContext({
      colorScheme: "light",
      deviceScaleFactor: 1,
      locale: "en-US",
      reducedMotion: "reduce",
      timezoneId: "UTC",
      viewport: { width: options.viewport.width, height: options.viewport.height },
    });
    const page = await context.newPage();
    await page.goto(options.url, { waitUntil: "load" });
    await page.addStyleTag({ content: DISABLE_MOTION_CSS });
    await page.evaluate(async () => {
      await document.fonts.ready;
    });
    const locator = page.locator(options.selector);
    const count = await locator.count();
    if (count !== 1) {
      throw new SelectorMatchError(options.selector, count);
    }
    const png = await locator.screenshot({ animations: "disabled", caret: "hide", scale: "css", type: "png" });
    const { width, height } = decodePng(png);
    return {
      png,
      sha256: sha256(png),
      width,
      height,
      settings: {
        animations: "disabled",
        browser: { args: [...CHROMIUM_ARGS], name: "chromium", version: browser.version() },
        caret: "hidden",
        colorScheme: "light",
        deviceScaleFactor: 1,
        disableMotionCss: DISABLE_MOTION_CSS,
        fontsReady: true,
        headless: true,
        locale: "en-US",
        playwright: playwrightVersion(),
        reducedMotion: "reduce",
        timezoneId: "UTC",
      },
    };
  } finally {
    await browser.close();
  }
}

/** The JSON manifest written next to a capture as `<name>.manifest.json`. */
export interface CaptureManifest {
  image: { file: string; height: number; sha256: string; width: number };
  kind: "figmog-shot/capture";
  selector: string;
  settings: CaptureSettings;
  url: string;
  version: 1;
  viewport: Viewport;
}

/**
 * Captures and writes `<outDir>/<name>.png` plus
 * `<outDir>/<name>.manifest.json`.
 */
export async function captureToDir(
  options: CaptureOptions & { outDir: string; name?: string },
): Promise<{ manifest: CaptureManifest; pngPath: string; manifestPath: string }> {
  const name = options.name ?? "capture";
  const result = await capture(options);
  await mkdir(options.outDir, { recursive: true });
  const pngPath = join(options.outDir, `${name}.png`);
  const manifestPath = join(options.outDir, `${name}.manifest.json`);
  const manifest: CaptureManifest = {
    image: { file: `${name}.png`, height: result.height, sha256: result.sha256, width: result.width },
    kind: "figmog-shot/capture",
    selector: options.selector,
    settings: result.settings,
    url: options.url,
    version: 1,
    viewport: options.viewport,
  };
  await writeFile(pngPath, result.png);
  await writeFile(manifestPath, stableJson(manifest));
  return { manifest, pngPath, manifestPath };
}
