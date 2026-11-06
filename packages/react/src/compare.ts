import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import pixelmatch from "pixelmatch";
import type { CaptureManifest } from "./capture.js";
import { sha256, stableJson } from "./digest.js";
import { drawText, GLYPH_HEIGHT, textWidth } from "./font.js";
import { blit, fillRect, readPng, solid, writePng, type Rgba } from "./png.js";

type Color = readonly [number, number, number, number];

/** pixelmatch per-pixel colour threshold (0..1, smaller is stricter). */
export const PIXEL_THRESHOLD = 0.1;
/** Red: pixels that differ inside the area both images cover. */
export const DIFF_COLOR: Color = [255, 0, 0, 255];
/** Yellow: anti-aliasing differences pixelmatch detects and does not count. */
export const AA_COLOR: Color = [255, 255, 0, 255];
/** Magenta: pixels only one image covers (size mismatch). */
export const SIZE_MISMATCH_COLOR: Color = [255, 0, 255, 255];
/** Grey: the unused part of a smaller image's panel in the composite. */
export const PANEL_PAD_COLOR: Color = [204, 204, 204, 255];
const BACKGROUND: Color = [255, 255, 255, 255];
const TEXT_COLOR: Color = [32, 32, 32, 255];
const TRANSPARENT: Color = [0, 0, 0, 0];

/** Composite panel order, left to right. */
export const LEGEND = ["figma", "before", "after", "difference"] as const;
const LABELS = ["FIGMA", "BEFORE", "AFTER", "DIFFERENCE"] as const;

export const SCORE_FORMULA =
  "score = round2(100 * (1 - mismatched / (W * H))), where W x H is the larger width x the larger height of " +
  "the two images, mismatched counts pixels pixelmatch (threshold 0.1, anti-aliasing ignored) finds different " +
  "inside the area both images cover, plus every pixel only one image covers (or neither does); round2 rounds " +
  "to two decimals; 100 means pixel-identical. Advisory only.";

export interface DiffResult {
  /**
   * `W x H` difference image: red differs, yellow anti-aliasing (not
   * counted), magenta size mismatch, faded grey matches.
   */
  image: Rgba;
  mismatched: number;
  total: number;
  score: number;
}

/** Pads `image` to `width` x `height`, top-left aligned, with `color`. */
function padTo(image: Rgba, width: number, height: number, color: Color): Rgba {
  const out = solid(width, height, color);
  blit(out, image, 0, 0);
  return out;
}

/** Compares `reference` against `candidate` under the documented size rule. */
export function diffImages(reference: Rgba, candidate: Rgba): DiffResult {
  const width = Math.max(reference.width, candidate.width);
  const height = Math.max(reference.height, candidate.height);
  const commonWidth = Math.min(reference.width, candidate.width);
  const commonHeight = Math.min(reference.height, candidate.height);
  const ignoreMask = new Uint8Array(width * height);
  let outside = 0;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (x >= commonWidth || y >= commonHeight) {
        ignoreMask[y * width + x] = 1;
        outside++;
      }
    }
  }
  const image = solid(width, height, TRANSPARENT);
  const inside = pixelmatch(
    padTo(reference, width, height, TRANSPARENT).data,
    padTo(candidate, width, height, TRANSPARENT).data,
    image.data,
    width,
    height,
    {
      aaColor: [AA_COLOR[0], AA_COLOR[1], AA_COLOR[2]],
      diffColor: [DIFF_COLOR[0], DIFF_COLOR[1], DIFF_COLOR[2]],
      ignoreMask,
      includeAA: false,
      threshold: PIXEL_THRESHOLD,
    },
  );
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (x >= commonWidth || y >= commonHeight) {
        fillRect(image, x, y, 1, 1, SIZE_MISMATCH_COLOR);
      }
    }
  }
  const mismatched = inside + outside;
  const total = width * height;
  const score = total === 0 ? 100 : Math.round(10000 * (1 - mismatched / total)) / 100;
  return { image, mismatched, total, score };
}

const MARGIN = 16;
const LABEL_SCALE = 2;
const LABEL_BAND = GLYPH_HEIGHT * LABEL_SCALE + 12;

/**
 * Lays out Figma | before | after | difference left to right, each in a
 * column as wide as the widest panel (and its label), labelled above.
 */
export function composite(panels: readonly [Rgba, Rgba, Rgba, Rgba]): Rgba {
  const panelWidth = Math.max(...panels.map((p) => p.width));
  const panelHeight = Math.max(...panels.map((p) => p.height));
  const column = Math.max(panelWidth, ...LABELS.map((l) => textWidth(l, LABEL_SCALE)));
  const width = MARGIN + LABELS.length * (column + MARGIN);
  const height = MARGIN + LABEL_BAND + panelHeight + MARGIN;
  const out = solid(width, height, BACKGROUND);
  panels.forEach((panel, index) => {
    const x = MARGIN + index * (column + MARGIN);
    drawText(out, LABELS[index] ?? "", x, MARGIN, LABEL_SCALE, column, TEXT_COLOR);
    fillRect(out, x, MARGIN + LABEL_BAND, panelWidth, panelHeight, PANEL_PAD_COLOR);
    blit(out, panel, x, MARGIN + LABEL_BAND);
  });
  return out;
}

export interface CompareOptions {
  figma: string;
  before: string;
  after: string;
  outDir: string;
}

interface ImageEntry {
  file: string;
  height: number;
  sha256: string;
  width: number;
}

interface InputEntry extends ImageEntry {
  /** The capture manifest found next to the input (`<name>.manifest.json`), if any. */
  capture: CaptureManifest | null;
}

/** The JSON manifest written as `<outDir>/manifest.json`. */
export interface CompareManifest {
  images: {
    composite: ImageEntry;
    diffAfter: ImageEntry;
    diffBefore: ImageEntry;
  };
  inputs: { after: InputEntry; before: InputEntry; figma: InputEntry };
  kind: "figmog-shot/compare";
  legend: {
    colors: { antiAliasing: number[]; diff: number[]; panelPadding: number[]; sizeMismatch: number[] };
    compositeOrder: string[];
    differencePanel: string;
  };
  score: {
    advisory: true;
    after: { mismatched: number; score: number; total: number };
    before: { mismatched: number; score: number; total: number };
    formula: string;
  };
  settings: { includeAA: false; sizeRule: string; threshold: number };
  version: 1;
  viewport: { height: number; width: number } | null;
}

async function loadCaptureManifest(pngPath: string): Promise<CaptureManifest | null> {
  const stem = basename(pngPath).replace(/\.png$/i, "");
  try {
    return JSON.parse(await readFile(join(dirname(pngPath), `${stem}.manifest.json`), "utf8")) as CaptureManifest;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

function entry(file: string, bytes: Uint8Array, image: Rgba): ImageEntry {
  return { file, height: image.height, sha256: sha256(bytes), width: image.width };
}

/**
 * Writes into `outDir`: copies of the three inputs (`figma.png`,
 * `before.png`, `after.png`), `diff-before.png` and `diff-after.png` (each
 * against Figma), `composite.png` (Figma | before | after | after-vs-Figma
 * difference) and `manifest.json`.
 */
export async function compareToDir(options: CompareOptions): Promise<{ manifest: CompareManifest; manifestPath: string }> {
  const [figma, before, after] = await Promise.all([readPng(options.figma), readPng(options.before), readPng(options.after)]);
  const [figmaCapture, beforeCapture, afterCapture] = await Promise.all([
    loadCaptureManifest(options.figma),
    loadCaptureManifest(options.before),
    loadCaptureManifest(options.after),
  ]);
  const diffBefore = diffImages(figma.image, before.image);
  const diffAfter = diffImages(figma.image, after.image);
  const compositeImage = composite([figma.image, before.image, after.image, diffAfter.image]);

  await mkdir(options.outDir, { recursive: true });
  await Promise.all([
    copyFile(options.figma, join(options.outDir, "figma.png")),
    copyFile(options.before, join(options.outDir, "before.png")),
    copyFile(options.after, join(options.outDir, "after.png")),
  ]);
  const diffBeforeBytes = await writePng(join(options.outDir, "diff-before.png"), diffBefore.image);
  const diffAfterBytes = await writePng(join(options.outDir, "diff-after.png"), diffAfter.image);
  const compositeBytes = await writePng(join(options.outDir, "composite.png"), compositeImage);

  const viewports = [figmaCapture, beforeCapture, afterCapture]
    .filter((c): c is CaptureManifest => c !== null)
    .map((c) => c.viewport);
  const first = viewports[0];
  const viewport =
    first !== undefined && viewports.every((v) => v.width === first.width && v.height === first.height)
      ? { height: first.height, width: first.width }
      : null;

  const manifest: CompareManifest = {
    images: {
      composite: entry("composite.png", compositeBytes, compositeImage),
      diffAfter: entry("diff-after.png", diffAfterBytes, diffAfter.image),
      diffBefore: entry("diff-before.png", diffBeforeBytes, diffBefore.image),
    },
    inputs: {
      after: { ...entry("after.png", after.bytes, after.image), capture: afterCapture },
      before: { ...entry("before.png", before.bytes, before.image), capture: beforeCapture },
      figma: { ...entry("figma.png", figma.bytes, figma.image), capture: figmaCapture },
    },
    kind: "figmog-shot/compare",
    legend: {
      colors: {
        antiAliasing: [...AA_COLOR],
        diff: [...DIFF_COLOR], panelPadding: [...PANEL_PAD_COLOR], sizeMismatch: [...SIZE_MISMATCH_COLOR] },
      compositeOrder: [...LEGEND],
      differencePanel: "after vs figma (diff-after.png)",
    },
    score: {
      advisory: true,
      after: { mismatched: diffAfter.mismatched, score: diffAfter.score, total: diffAfter.total },
      before: { mismatched: diffBefore.mismatched, score: diffBefore.score, total: diffBefore.total },
      formula: SCORE_FORMULA,
    },
    settings: {
      includeAA: false,
      sizeRule: "images are aligned top-left and padded to the larger width and height; padded pixels count as mismatched",
      threshold: PIXEL_THRESHOLD,
    },
    version: 1,
    viewport,
  };
  const manifestPath = join(options.outDir, "manifest.json");
  await writeFile(manifestPath, stableJson(manifest));
  return { manifest, manifestPath };
}
