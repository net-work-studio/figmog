// Automatic `data-figmog` / `data-figmog-node` attributes (NWS-2957): adds
// them to the root element of a mapped component's JSX, in development
// builds only. Fail-safe: a component whose root cannot be pinned to one
// DOM element is left untouched and reported, never half-marked.
import { parseAst } from "vite";

/** The attribute naming the map entry: `<component|component_set>:<key>`. */
export const FIGMOG_ATTRIBUTE = "data-figmog";
/** The attribute naming the Figma node (when the mirror knows exactly one). */
export const NODE_ATTRIBUTE = "data-figmog-node";

/** One export of a module to mark. */
export interface MarkSpec {
  /** The exported component's name (`code.export` in figmog.map.json). */
  exportName: string;
  /** `data-figmog`'s value. */
  figma: string;
  /** `data-figmog-node`'s value, or `null` to leave it out. */
  node: string | null;
}

export interface MarkOutcome {
  /** The marked source, or `null` when nothing changed. */
  code: string | null;
  /** Exports that were marked, sorted. */
  marked: string[];
  /** Exports left unmarked, with the reason, sorted by export. */
  skipped: { exportName: string; reason: string }[];
}

interface Node {
  type: string;
  start: number;
  end: number;
}

function isNode(value: unknown): value is Node {
  return typeof value === "object" && value !== null && typeof (value as { type?: unknown }).type === "string";
}

function field(node: Node, key: string): unknown {
  return (node as unknown as Record<string, unknown>)[key];
}

function child(node: Node, key: string): Node | null {
  const value = field(node, key);
  return isNode(value) ? value : null;
}

function list(node: Node, key: string): Node[] {
  const value = field(node, key);
  return Array.isArray(value) ? value.filter(isNode) : [];
}

function name(node: Node | null): string | null {
  if (node === null) return null;
  if (node.type === "Identifier" || node.type === "JSXIdentifier") {
    const value = field(node, "name");
    return typeof value === "string" ? value : null;
  }
  if (node.type === "Literal") {
    const value = field(node, "value");
    return typeof value === "string" ? value : null;
  }
  return null;
}

const FUNCTIONS = new Set(["ArrowFunctionExpression", "FunctionDeclaration", "FunctionExpression"]);
/** Expression wrappers that do not change what is rendered. */
const WRAPPERS = new Set([
  "ParenthesizedExpression",
  "TSAsExpression",
  "TSNonNullExpression",
  "TSSatisfiesExpression",
  "TSTypeAssertion",
]);
/** `memo(...)` / `forwardRef(...)` wrap a component without changing its root. */
const COMPONENT_WRAPPERS = new Set(["forwardRef", "memo"]);

function unwrap(node: Node): Node {
  let current = node;
  while (WRAPPERS.has(current.type)) {
    const inner = child(current, "expression");
    if (inner === null) break;
    current = inner;
  }
  return current;
}

type ComponentResolution = { fn: Node; reason: null } | { fn: null; reason: string };

/** Resolve only bindings whose declaration still describes their value. */
function componentFunction(
  value: string,
  locals: ReadonlyMap<string, Node>,
  writes: ReadonlySet<string>,
): ComponentResolution {
  const seen = new Set<Node>();
  let current: Node | string = value;
  while (true) {
    if (typeof current === "string") {
      if (writes.has(current)) return { fn: null, reason: `binding ${current} may be reassigned; mark it explicitly` };
      const local = locals.get(current);
      if (local === undefined) return { fn: null, reason: "not a function component" };
      current = local;
    }
    const node = unwrap(current);
    if (seen.has(node)) return { fn: null, reason: "not a function component" };
    seen.add(node);
    if (FUNCTIONS.has(node.type)) return { fn: node, reason: null };
    if (node.type === "Identifier") {
      const id = name(node);
      if (id === null) return { fn: null, reason: "not a function component" };
      current = id;
      continue;
    }
    if (node.type === "CallExpression") {
      const callee = child(node, "callee");
      const calleeName =
        callee?.type === "MemberExpression" ? name(child(callee, "property")) : name(callee);
      const first = list(node, "arguments")[0];
      if (calleeName !== null && COMPONENT_WRAPPERS.has(calleeName) && first !== undefined) {
        current = first;
        continue;
      }
    }
    return { fn: null, reason: "not a function component" };
  }
}

/** Top-level bindings and exports of a module. */
function bindings(program: Node): { locals: Map<string, Node>; exports: Map<string, string> } {
  const locals = new Map<string, Node>();
  const exports = new Map<string, string>();
  const declare = (declaration: Node, exported: boolean): void => {
    if (declaration.type === "FunctionDeclaration") {
      const id = name(child(declaration, "id"));
      if (id !== null) {
        locals.set(id, declaration);
        if (exported) exports.set(id, id);
      }
    } else if (declaration.type === "VariableDeclaration") {
      for (const declarator of list(declaration, "declarations")) {
        const id = name(child(declarator, "id"));
        const init = child(declarator, "init");
        if (id !== null && init !== null) {
          locals.set(id, init);
          if (exported) exports.set(id, id);
        }
      }
    }
  };
  for (const statement of list(program, "body")) {
    if (statement.type === "ExportNamedDeclaration") {
      const declaration = child(statement, "declaration");
      if (declaration !== null) declare(declaration, true);
      if (child(statement, "source") === null) {
        for (const specifier of list(statement, "specifiers")) {
          const local = name(child(specifier, "local"));
          const exported = name(child(specifier, "exported"));
          if (local !== null && exported !== null) exports.set(exported, local);
        }
      }
    } else if (statement.type === "ExportDefaultDeclaration") {
      // `export default function PricingCard() {}`: matched by its own name.
      const declaration = child(statement, "declaration");
      if (declaration !== null && declaration.type === "FunctionDeclaration") {
        declare(declaration, false);
        const id = name(child(declaration, "id"));
        if (id !== null && !exports.has(id)) exports.set(id, id);
      }
    } else {
      declare(statement, false);
    }
  }
  return { locals, exports };
}

/** Names written by an assignment pattern; member properties do not rebind. */
function assignedNames(value: Node | null, out: Set<string>): void {
  if (value === null) return;
  const node = unwrap(value);
  const id = name(node);
  if (node.type === "Identifier" && id !== null) out.add(id);
  else if (node.type === "ObjectPattern") {
    for (const property of list(node, "properties")) {
      assignedNames(child(property, property.type === "RestElement" ? "argument" : "value"), out);
    }
  } else if (node.type === "ArrayPattern") {
    for (const element of list(node, "elements")) assignedNames(element, out);
  } else if (node.type === "RestElement" || node.type === "AssignmentPattern") {
    assignedNames(child(node, node.type === "RestElement" ? "argument" : "left"), out);
  }
}

/**
 * Fail closed on syntactic writes anywhere in the module, including closures
 * that might run before a wrapper is created. Repeated declarations also make
 * an initializer uncertain. Shadowed names may cause a conservative skip; we
 * never infer execution order or use a stale initializer to claim a JSX root.
 */
function writtenBindings(program: Node): Set<string> {
  const writes = new Set<string>();
  const declarations = new Set<string>();
  const visit = (node: Node): void => {
    let declared: Node | null = null;
    if (node.type === "VariableDeclarator" || node.type === "FunctionDeclaration") declared = child(node, "id");
    if (declared !== null) {
      const names = new Set<string>();
      assignedNames(declared, names);
      for (const id of names) {
        if (declarations.has(id)) writes.add(id);
        declarations.add(id);
      }
    }
    if (node.type === "AssignmentExpression") assignedNames(child(node, "left"), writes);
    else if (node.type === "UpdateExpression") assignedNames(child(node, "argument"), writes);
    else if (node.type === "ForInStatement" || node.type === "ForOfStatement") {
      assignedNames(child(node, "left"), writes);
    }
    for (const value of Object.values(node as unknown as Record<string, unknown>)) {
      if (Array.isArray(value)) value.filter(isNode).forEach(visit);
      else if (isNode(value)) visit(value);
    }
  };
  visit(program);
  return writes;
}

/** Every value a component function can return (its own `return`s only). */
function returned(fn: Node): (Node | null)[] {
  const body = child(fn, "body");
  if (body === null) return [];
  if (body.type !== "BlockStatement") return [body];
  const out: (Node | null)[] = [];
  const visit = (node: Node): void => {
    if (FUNCTIONS.has(node.type) || node.type === "ClassDeclaration" || node.type === "ClassExpression") return;
    if (node.type === "ReturnStatement") {
      out.push(child(node, "argument"));
      return;
    }
    for (const value of Object.values(node as unknown as Record<string, unknown>)) {
      if (Array.isArray(value)) value.filter(isNode).forEach(visit);
      else if (isNode(value)) visit(value);
    }
  };
  for (const statement of list(body, "body")) visit(statement);
  return out;
}

/**
 * The host elements a returned value renders as its root, or the reason it
 * has no single root element.
 */
function roots(value: Node | null, out: Node[]): string | null {
  if (value === null) return null;
  const node = unwrap(value);
  if (node.type === "Literal" && field(node, "value") === null) return null;
  if (node.type === "Identifier" && name(node) === "undefined") return null;
  if (node.type === "ConditionalExpression") {
    return roots(child(node, "consequent"), out) ?? roots(child(node, "alternate"), out);
  }
  if (node.type === "JSXFragment") return "its root is a fragment (no single root element)";
  if (node.type === "JSXElement") {
    const opening = child(node, "openingElement");
    const tag = opening === null ? null : child(opening, "name");
    const tagName = name(tag);
    if (opening !== null && tag?.type === "JSXIdentifier" && tagName !== null && /^[a-z]/.test(tagName)) {
      out.push(opening);
      return null;
    }
    return `its root is a component element, not a DOM element; mark it explicitly`;
  }
  return "its root is not a single JSX element";
}

function hasFigmogAttribute(opening: Node): boolean {
  return list(opening, "attributes").some((attribute) => {
    if (attribute.type !== "JSXAttribute") return false;
    const attributeName = name(child(attribute, "name"));
    return attributeName === FIGMOG_ATTRIBUTE || attributeName === NODE_ATTRIBUTE;
  });
}

function language(filename: string): "jsx" | "tsx" | null {
  if (/\.tsx$/.test(filename)) return "tsx";
  if (/\.(jsx|js|mjs)$/.test(filename)) return "jsx";
  return null;
}

/**
 * Adds `data-figmog` (and `data-figmog-node` when known) to the root element
 * of each component in `specs`. A component is skipped, with a reason, when it
 * is not found, is not a function component, returns a fragment, a component
 * element or anything else that is not one DOM element on every path, or
 * already carries the attributes (explicit markers win). Bindings with writes
 * or repeated declarations are skipped rather than following stale values.
 */
export function markModule(code: string, filename: string, specs: readonly MarkSpec[]): MarkOutcome {
  const skipped: { exportName: string; reason: string }[] = [];
  const marked: string[] = [];
  const lang = language(filename);
  if (lang === null) {
    for (const spec of specs) skipped.push({ exportName: spec.exportName, reason: "not a JSX or TSX module" });
    return { code: null, marked, skipped };
  }
  let program: Node;
  try {
    program = parseAst(code, { lang }) as unknown as Node;
  } catch (error) {
    const reason = `the module does not parse: ${error instanceof Error ? error.message.split("\n")[0] : String(error)}`;
    for (const spec of specs) skipped.push({ exportName: spec.exportName, reason });
    return { code: null, marked, skipped };
  }
  const { locals, exports } = bindings(program);
  const writes = writtenBindings(program);
  const inserts: { at: number; text: string }[] = [];
  // Resolve every export before editing: several export names can refer to
  // the same function (or return the same root). None can claim that element.
  const candidates = [...specs].sort((a, b) => a.exportName.localeCompare(b.exportName)).map((spec) => {
    const exported = exports.get(spec.exportName);
    const { fn, reason: resolutionReason } = exported === undefined
      ? { fn: null, reason: "no such export" }
      : componentFunction(exported, locals, writes);
    const elements: Node[] = [];
    let reason = resolutionReason;
    if (fn !== null) {
      for (const result of returned(fn)) {
        reason = roots(result, elements);
        if (reason !== null) break;
      }
    }
    return { spec, fn, elements, reason };
  });
  const functions = new Map<Node, number>();
  const elements = new Map<Node, number>();
  for (const candidate of candidates) {
    if (candidate.fn !== null) functions.set(candidate.fn, (functions.get(candidate.fn) ?? 0) + 1);
    for (const opening of new Set(candidate.elements)) {
      elements.set(opening, (elements.get(opening) ?? 0) + 1);
    }
  }
  for (const candidate of candidates) {
    const { spec, fn, elements: roots, reason } = candidate;
    const skip = (reason: string): void => {
      skipped.push({ exportName: spec.exportName, reason });
    };
    if ((fn !== null && (functions.get(fn) ?? 0) > 1) || roots.some((root) => (elements.get(root) ?? 0) > 1)) {
      skip("several figmog.map.json entries resolve to this component; mark it explicitly");
      continue;
    }
    if (reason !== null) {
      skip(reason);
      continue;
    }
    if (roots.length === 0) {
      skip("it renders no element");
      continue;
    }
    if (roots.some(hasFigmogAttribute)) {
      skip(`its root already has ${FIGMOG_ATTRIBUTE} attributes (explicit markers win)`);
      continue;
    }
    let text = ` ${FIGMOG_ATTRIBUTE}={${JSON.stringify(spec.figma)}}`;
    if (spec.node !== null) text += ` ${NODE_ATTRIBUTE}={${JSON.stringify(spec.node)}}`;
    for (const opening of roots) {
      // Caller props must override automatic defaults, including JSX spreads.
      const tag = child(opening, "name");
      if (tag !== null) inserts.push({ at: tag.end, text });
    }
    marked.push(spec.exportName);
  }
  if (inserts.length === 0) return { code: null, marked, skipped };
  let out = code;
  for (const { at, text } of inserts.sort((a, b) => b.at - a.at)) {
    out = out.slice(0, at) + text + out.slice(at);
  }
  return { code: out, marked, skipped };
}
