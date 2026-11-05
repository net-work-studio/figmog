#!/usr/bin/env node
import { parseArgs } from "node:util";
import { captureToDir, parseViewport } from "./capture.js";
import { compareToDir } from "./compare.js";
import { captureTarget } from "./target.js";

const USAGE = `usage:
  figmog-shot capture --url URL --selector CSS --viewport WIDTHxHEIGHT --out DIR [--name NAME]
  figmog-shot compare --figma FIGMA.png --before BEFORE.png --after AFTER.png --out DIR
  figmog-shot target --manifest figmog.targets.json --target NAME --base-url URL --out DIR [--viewport WIDTHxHEIGHT]...
`;

function required(values: Record<string, string | string[] | boolean | undefined>, name: string): string {
  const value = values[name];
  if (typeof value !== "string" || value === "") {
    throw new UsageError(`missing --${name}`);
  }
  return value;
}

class UsageError extends Error {}

async function main(argv: string[]): Promise<void> {
  const [command, ...rest] = argv;
  if (command === "capture") {
    const { values } = parseArgs({
      args: rest,
      options: {
        name: { type: "string" },
        out: { type: "string" },
        selector: { type: "string" },
        url: { type: "string" },
        viewport: { type: "string" },
      },
      strict: true,
    });
    const { manifestPath } = await captureToDir({
      outDir: required(values, "out"),
      selector: required(values, "selector"),
      url: required(values, "url"),
      viewport: parseViewport(required(values, "viewport")),
      ...(values.name === undefined ? {} : { name: values.name }),
    });
    process.stdout.write(`${manifestPath}\n`);
  } else if (command === "compare") {
    const { values } = parseArgs({
      args: rest,
      options: {
        after: { type: "string" },
        before: { type: "string" },
        figma: { type: "string" },
        out: { type: "string" },
      },
      strict: true,
    });
    const { manifestPath } = await compareToDir({
      after: required(values, "after"),
      before: required(values, "before"),
      figma: required(values, "figma"),
      outDir: required(values, "out"),
    });
    process.stdout.write(`${manifestPath}\n`);
  } else if (command === "target") {
    const { values } = parseArgs({
      args: rest,
      options: {
        "base-url": { type: "string" },
        manifest: { type: "string" },
        out: { type: "string" },
        target: { type: "string" },
        viewport: { type: "string", multiple: true },
      },
      strict: true,
    });
    const { summaryPath } = await captureTarget({
      baseUrl: required(values, "base-url"),
      manifest: required(values, "manifest"),
      outDir: required(values, "out"),
      target: required(values, "target"),
      ...(values.viewport === undefined ? {} : { viewports: values.viewport.map(parseViewport) }),
    });
    process.stdout.write(`${summaryPath}\n`);
  } else if (command === "--help" || command === "-h" || command === "help") {
    process.stdout.write(USAGE);
  } else {
    throw new UsageError(command === undefined ? "missing command" : `unknown command ${JSON.stringify(command)}`);
  }
}

main(process.argv.slice(2)).catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`figmog-shot: ${message}\n`);
  if (error instanceof UsageError || (error instanceof TypeError && "code" in error)) {
    process.stderr.write(USAGE);
    process.exitCode = 2;
  } else {
    process.exitCode = 1;
  }
});
