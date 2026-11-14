import { readFile, writeFile } from "node:fs/promises";
import { PNG } from "pngjs";

/** Decoded RGBA image: `data` holds `width * height * 4` bytes. */
export interface Rgba {
  width: number;
  height: number;
  data: Uint8Array;
}

export function decodePng(bytes: Uint8Array): Rgba {
  const png = PNG.sync.read(Buffer.from(bytes));
  return { width: png.width, height: png.height, data: new Uint8Array(png.data) };
}

/**
 * Encodes as 8-bit RGBA with fixed encoder options; pngjs writes no
 * timestamps or other ambient metadata, so equal pixels give equal bytes.
 */
export function encodePng(image: Rgba): Buffer {
  const png = new PNG({ width: image.width, height: image.height });
  png.data = Buffer.from(image.data);
  return PNG.sync.write(png, { colorType: 6, inputColorType: 6, bitDepth: 8, deflateLevel: 9 });
}

export async function readPng(path: string): Promise<{ bytes: Buffer; image: Rgba }> {
  const bytes = await readFile(path);
  return { bytes, image: decodePng(bytes) };
}

export async function writePng(path: string, image: Rgba): Promise<Buffer> {
  const bytes = encodePng(image);
  await writeFile(path, bytes);
  return bytes;
}

/** A `width` x `height` image filled with one RGBA colour. */
export function solid(width: number, height: number, rgba: readonly [number, number, number, number]): Rgba {
  const data = new Uint8Array(width * height * 4);
  for (let i = 0; i < data.length; i += 4) {
    data[i] = rgba[0];
    data[i + 1] = rgba[1];
    data[i + 2] = rgba[2];
    data[i + 3] = rgba[3];
  }
  return { width, height, data };
}

/** Copies `src` into `dst` with its top-left corner at (`x`, `y`). */
export function blit(dst: Rgba, src: Rgba, x: number, y: number): void {
  for (let row = 0; row < src.height; row++) {
    const from = row * src.width * 4;
    dst.data.set(src.data.subarray(from, from + src.width * 4), ((y + row) * dst.width + x) * 4);
  }
}

/** Fills a rectangle of `dst` with one RGBA colour. */
export function fillRect(
  dst: Rgba,
  x: number,
  y: number,
  width: number,
  height: number,
  rgba: readonly [number, number, number, number],
): void {
  for (let row = y; row < y + height; row++) {
    for (let col = x; col < x + width; col++) {
      const i = (row * dst.width + col) * 4;
      dst.data[i] = rgba[0];
      dst.data[i + 1] = rgba[1];
      dst.data[i + 2] = rgba[2];
      dst.data[i + 3] = rgba[3];
    }
  }
}
