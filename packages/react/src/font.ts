import { fillRect, type Rgba } from "./png.js";

/**
 * A 5x7 bitmap font covering only the letters of the composite's panel
 * labels, so labels can be drawn without a font file or a text renderer.
 */
const GLYPHS: Readonly<Record<string, readonly string[]>> = {
  A: [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
  B: ["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."],
  C: [".###.", "#...#", "#....", "#....", "#....", "#...#", ".###."],
  D: ["####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####."],
  E: ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
  F: ["#####", "#....", "#....", "####.", "#....", "#....", "#...."],
  G: [".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".###."],
  I: ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
  M: ["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"],
  N: ["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"],
  O: [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
  R: ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
  T: ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
};

export const GLYPH_HEIGHT = 7;
const GLYPH_ADVANCE = 6;

/** Width in pixels of `text` drawn at `scale`. */
export function textWidth(text: string, scale: number): number {
  return text.length === 0 ? 0 : (text.length * GLYPH_ADVANCE - 1) * scale;
}

/** Draws upper-case `text` into `dst`, clipped to `maxWidth` pixels. */
export function drawText(
  dst: Rgba,
  text: string,
  x: number,
  y: number,
  scale: number,
  maxWidth: number,
  rgba: readonly [number, number, number, number],
): void {
  [...text].forEach((ch, index) => {
    const glyph = GLYPHS[ch];
    if (glyph === undefined) {
      throw new Error(`no glyph for ${JSON.stringify(ch)}`);
    }
    glyph.forEach((row, gy) => {
      [...row].forEach((cell, gx) => {
        const px = (index * GLYPH_ADVANCE + gx) * scale;
        if (cell === "#" && px + scale <= maxWidth) {
          fillRect(dst, x + px, y + gy * scale, scale, scale, rgba);
        }
      });
    });
  });
}
