#!/usr/bin/env node
// Dependency-free lint for this package. Type-level rules (unused locals and
// parameters, implicit any, unchecked index access, ...) are enforced by
// `npm run typecheck` through tsconfig.json; this script adds the source
// hygiene rules tsc does not cover. Exits non-zero listing every violation.
import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const dirs = ["src", "test", "scripts"];
const extensions = /\.(ts|mjs|html|css)$/;
const MAX_LINE = 140;

/** [rule, applies-to, pattern] checked against each line. */
const lineRules = [
  ["no-tabs", /./, /\t/],
  ["no-trailing-whitespace", /./, /[ \t]+$/],
  ["no-explicit-any", /\.ts$/, /(:\s*any\b|\bas\s+any\b|<any>)/],
  ["no-console", /^src\//, /\bconsole\.(log|debug|info|warn|error)\(/],
  ["no-focused-or-skipped-tests", /^test\//, /\b(test|describe|it)\.(only|skip)\(/],
  ["no-debugger", /./, /^\s*debugger\s*;?\s*$/],
  ["no-ts-comment-suppression", /\.ts$/, /@ts-(ignore|nocheck|expect-error)/],
  ["relative-imports-use-js-extension", /\.ts$/, /from\s+"\.{1,2}\/[^"]*(?<!\.js)"/],
];

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(path));
    else if (extensions.test(entry.name)) out.push(path);
  }
  return out;
}

const problems = [];
for (const file of dirs.flatMap((d) => walk(join(root, d)))) {
  const rel = relative(root, file).split("\\").join("/");
  const text = readFileSync(file, "utf8");
  if (text.length > 0 && !text.endsWith("\n")) problems.push(`${rel}: missing final newline [final-newline]`);
  if (text.includes("\r")) problems.push(`${rel}: CRLF line endings [lf-only]`);
  text.split("\n").forEach((line, index) => {
    if (line.length > MAX_LINE) problems.push(`${rel}:${index + 1}: line longer than ${MAX_LINE} [max-line-length]`);
    for (const [rule, scope, pattern] of lineRules) {
      if (scope.test(rel) && pattern.test(line)) problems.push(`${rel}:${index + 1}: ${rule}`);
    }
  });
}

if (problems.length > 0) {
  process.stderr.write(`${problems.join("\n")}\n${problems.length} lint problem(s)\n`);
  process.exit(1);
}
process.stdout.write("lint: ok\n");
