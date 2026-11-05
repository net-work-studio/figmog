#!/usr/bin/env node
// A stand-in figmog binary for the plugin and bridge tests: no Rust build
// needed. `settings`, `map check` and `map coverage` answer from the working
// directory (`figmog.toml` is not read: the fake's [files] are fixed;
// `fake-nodes.json` holds coverage's node ids); `serve` is a stdio relay that
// answers every tools/call with the call it received, appends each frame to
// FAKE_FIGMOG_LOG, and exits with code 3 when asked to (`{"exit": true}`).
import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const [command, sub] = process.argv.slice(2);
const cwd = process.cwd();
const FILES = { default: "FakeProductFile01", library: "FakeLibraryFile01" };

function json(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

if (command === "settings") {
  json({
    project_root: cwd,
    config: null,
    files: FILES,
    unknown_keys: [],
    inspector: { enabled: true, shortcut: "Alt+Shift+F", attributes: process.env["FAKE_FIGMOG_ATTRIBUTES"] === "1" },
  });
} else if (command === "map" && sub === "check") {
  const map = JSON.parse(readFileSync("figmog.map.json", "utf8"));
  json({ map: "figmog.map.json", components: map.components.length });
} else if (command === "map" && sub === "coverage") {
  const map = JSON.parse(readFileSync("figmog.map.json", "utf8"));
  const nodes = existsSync("fake-nodes.json") ? JSON.parse(readFileSync("fake-nodes.json", "utf8")) : {};
  const mapped = map.components.map((entry) => {
    const kind = "component_set" in entry.figma ? "component_set" : "component";
    const key = entry.figma[kind];
    return { figma: { kind, key, name: key, node_ids: nodes[`${kind}:${key}`] ?? [] }, code: entry.code };
  });
  json({ map: { path: "figmog.map.json", present: true }, mapped, not_in_mirror: [], unmapped: [] });
} else if (command === "serve") {
  const log = process.env["FAKE_FIGMOG_LOG"];
  createInterface({ input: process.stdin }).on("line", (line) => {
    if (log) appendFileSync(log, `${line}\n`);
    const frame = JSON.parse(line);
    if (frame.params?.arguments?.exit === true) process.exit(3);
    const text = JSON.stringify({ arguments: frame.params?.arguments ?? null, tool: frame.params?.name ?? null });
    json({ jsonrpc: "2.0", id: frame.id, result: { content: [{ type: "text", text }] } });
  });
} else {
  process.stderr.write(`${JSON.stringify({ error: `fake figmog: unsupported ${process.argv.slice(2).join(" ")}` })}\n`);
  process.exit(1);
}
