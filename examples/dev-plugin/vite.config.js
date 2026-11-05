import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";
// In a real project: import { figmog } from "@figmog/react/vite";
import { figmog } from "../../packages/react/dist/src/vite.js";

/**
 * The sample app's config. `figmog()` must come before plugins that compile
 * JSX. The tests call this with plugin options (the figmog binary, relay flags).
 */
export function config(figmogOptions = {}) {
  return defineConfig({
    plugins: [figmog(figmogOptions), tailwindcss()],
  });
}

// The sample's "Figma file" is an offline fixture: the bridge's relay must not
// ask figmog to watch it for changes on Figma.
export default config({ serveArgs: ["--no-watch"] });
