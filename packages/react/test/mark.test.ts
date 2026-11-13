import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { transformWithOxc } from "vite";
import { markModule, type MarkSpec } from "../src/mark.js";

const SPEC: MarkSpec = { exportName: "Card", figma: "component_set:abc", node: "10:1" };
const ATTRS = ' data-figmog={"component_set:abc"} data-figmog-node={"10:1"}';

function mark(code: string, specs: MarkSpec[] = [SPEC], file = "Card.tsx") {
  return markModule(code, file, specs);
}

describe("markModule", () => {
  test("marks the root element of a function declaration, arrow and default export", () => {
    const cases: [string, string][] = [
      ["export function Card() { return <div className=\"c\">x</div>; }", `<div${ATTRS} className="c">x</div>`],
      ["export const Card = () => (<section>x</section>);", `<section${ATTRS}>x</section>`],
      ["export const Card = (p: {a: string}) => <hr />;", `<hr${ATTRS} />`],
      ["export default function Card() { return <main/>; }", `<main${ATTRS}/>`],
      ["function Local() { return <i>é</i>; }\nexport { Local as Card };", `<i${ATTRS}>é</i>`],
      ["export const Card = memo(forwardRef((p, ref) => <b ref={ref} />));", `<b${ATTRS} ref={ref} />`],
      ["export const Card = React.memo(function Card() { return <p>x</p>; });", `<p${ATTRS}>x</p>`],
    ];
    for (const [code, expected] of cases) {
      const outcome = mark(code);
      assert.deepEqual(outcome.marked, ["Card"], code);
      assert.ok(outcome.code?.includes(expected), `${code}\n=> ${outcome.code}`);
    }
  });

  test("resolves local component functions inside memo and forwardRef wrappers", () => {
    const cases = [
      ...["memo", "forwardRef", "React.memo", "React.forwardRef"].map((wrapper) =>
        `function Inner() { return <div/>; } export const Card = ${wrapper}(Inner);`,
      ),
      "const Inner = () => <div/>; const Alias = Inner; export const Card = React.memo(React.forwardRef(Alias));",
      "const Inner = () => <div/>; const Wrapped = forwardRef(Inner); export const Card = memo(Wrapped);",
      "export const Card = memo(Inner); function Inner() { return <div/>; }",
      "export default function Inner() { return <div/>; } export const Card = memo(Inner);",
      "function Inner() { return <div/>; } const Alias = Inner; export const Card = memo((Alias as typeof Inner)!);",
    ];
    for (const code of cases) {
      const outcome = mark(code);
      assert.deepEqual(outcome.skipped, [], code);
      assert.deepEqual(outcome.marked, ["Card"], code);
      assert.ok(outcome.code?.includes(`<div${ATTRS}/>`), `${code}\n=> ${outcome.code}`);
    }
  });

  test("skips every mapping that resolves through wrappers to the same component", () => {
    const cases = [
      "export function Card() { return <div/>; } export const Wrapped = memo(Card);",
      "export const Card = () => <div/>; export const Wrapped = React.forwardRef(Card);",
      "const Inner = () => <div/>; export const Card = memo(Inner); export const Wrapped = forwardRef(Inner);",
      "export default function Card() { return <div/>; } export const Wrapped = React.memo(Card);",
    ];
    const specs = [SPEC, { exportName: "Wrapped", figma: "component:wrapped", node: "2:1" }];
    for (const code of cases) {
      const outcome = mark(code, specs);
      assert.equal(outcome.code, null, code);
      assert.deepEqual(outcome.marked, [], code);
      assert.deepEqual(outcome.skipped.map(({ exportName }) => exportName), ["Card", "Wrapped"], code);
      for (const { reason } of outcome.skipped) assert.match(reason, /several .* entries resolve to this component/);
      assert.deepEqual(mark(code, [...specs].reverse()), outcome);
    }
  });

  test("skips reassigned wrapper arguments and aliases without marking stale functions", async () => {
    const functions = "function Initial() { return <div/>; } function Actual() { return <span/>; } ";
    const cases = [
      "let Inner = Initial; Inner = Actual; export const Card = memo(Inner);",
      "var Inner = Initial; Inner = Actual; export const Card = React.forwardRef(Inner);",
      "let Inner = Initial; const Alias = Inner; Inner = Actual; export const Card = memo(Alias);",
      "let Inner = Initial; const Alias = memo(Inner); Inner = Actual; export const Card = memo(Alias);",
      "Initial = Actual; export const Card = memo(Initial);",
      "let Inner = Initial; Inner ||= Actual; export const Card = memo(Inner);",
      "var Inner = Initial; var Inner = Actual; export const Card = memo(Inner);",
      "var Inner = Initial; var Inner; export const Card = memo(Inner);",
      "let Inner = Initial; ({ value: Inner } = { value: Actual }); export const Card = memo(Inner);",
      "let Inner = Initial; [Inner] = [Actual]; export const Card = memo(Inner);",
      "let Inner = Initial; ({ value: Inner = Actual } = {}); export const Card = memo(Inner);",
      "let Inner = Initial; [Inner, ...Inner] = [Actual]; export const Card = memo(Inner);",
      "let Inner = Initial; function replace() { Inner = Actual; } export const Card = memo(Inner);",
      "let Inner = Initial; for (Inner of [Actual]) {} export const Card = memo(Inner);",
      "var Inner = Initial; for (var Inner of [Actual]) {} export const Card = memo(Inner);",
      "let Inner = Initial; for (Inner in {}) {} export const Card = memo(Inner);",
      "let Inner = Initial; for ({ value: Inner } of [{ value: Actual }]) {} export const Card = memo(Inner);",
      "let Inner = Initial; Inner++; export const Card = memo(Inner);",
      "export let Card = Initial; Card = Actual;",
      "let Inner = Initial; Inner = Actual; export { Inner as Card };",
    ];
    for (const code of cases) {
      const outcome = mark(functions + code);
      assert.equal(outcome.code, null, code);
      assert.deepEqual(outcome.marked, [], code);
      assert.equal(outcome.skipped.length, 1, code);
      assert.match(outcome.skipped[0]?.reason ?? "", /binding .* may be reassigned/, code);
    }
    const source = functions + cases[0];
    const outcome = mark(source);
    const compiled = await transformWithOxc(outcome.code ?? source, "Card.jsx", {
      jsx: { development: false, pragma: "h", runtime: "classic" },
    });
    const components = new Function("h", "memo", `${compiled.code.replace("export const Card", "const Card")}\n` +
      "return { Card, Initial };")(
      (tag: string, props: Record<string, unknown> | null) => ({ tag, props }),
      (component: unknown) => component,
    ) as Record<string, () => { tag: string; props: Record<string, unknown> | null }>;
    assert.deepEqual(components.Card?.(), { tag: "span", props: null });
    assert.deepEqual(components.Initial?.(), { tag: "div", props: null });

    const mixed = mark(source + " export const Panel = () => <aside/>;", [
      SPEC, { exportName: "Panel", figma: "component:panel", node: null },
    ]);
    assert.deepEqual(mixed.marked, ["Panel"]);
    assert.deepEqual(mixed.skipped.map(({ exportName }) => exportName), ["Card"]);
    assert.equal(mixed.code, source + ' export const Panel = () => <aside data-figmog={"component:panel"}/>;');
  });

  test("follows stable mutable locals and ignores writes to their properties", () => {
    for (const declaration of ["let", "var"]) {
      const outcome = mark(`function Inner() { return <div/>; } ${declaration} Alias = Inner; ` +
        'Alias.displayName = "Card"; export const Card = memo(Alias);');
      assert.deepEqual(outcome.skipped, []);
      assert.deepEqual(outcome.marked, ["Card"]);
    }
  });

  test("skips missing, imported and cyclic wrapper arguments without following them forever", () => {
    const cases = [
      "export const Card = memo(Missing);",
      'import Inner from "./Inner.js"; export const Card = React.forwardRef(Inner);',
      "export const Card = memo(Card);",
      "const A = B; const B = A; export const Card = forwardRef(A);",
      "const A = memo(B); const B = forwardRef(A); export const Card = memo(A);",
    ];
    for (const code of cases) {
      const outcome = mark(code);
      assert.equal(outcome.code, null, code);
      assert.deepEqual(outcome.marked, [], code);
      assert.deepEqual(outcome.skipped, [{ exportName: "Card", reason: "not a function component" }], code);
    }
  });

  test("caller markers in spread props override automatic defaults", async () => {
    const outcome = mark("export const Card = props => <div {...props}/>;", [SPEC], "Card.jsx");
    assert.deepEqual(outcome.marked, ["Card"]);
    assert.ok(outcome.code?.includes(`<div${ATTRS} {...props}/>`));
    const compiled = await transformWithOxc(outcome.code ?? "", "Card.jsx", {
      jsx: { development: false, pragma: "h", runtime: "classic" },
    });
    // Evaluate Vite's actual JSX transform with an element factory that exposes
    // its final props, so the assertion checks spread precedence at runtime.
    const render = new Function("h", `${compiled.code.replace("export const Card", "const Card")}\nreturn Card;`)(
      (_tag: string, props: Record<string, unknown>) => props,
    ) as (props: Record<string, unknown>) => Record<string, unknown>;
    assert.deepEqual(render({}), { "data-figmog": SPEC.figma, "data-figmog-node": SPEC.node });
    const explicit = { "data-figmog": "component:explicit", "data-figmog-node": "2:1" };
    assert.deepEqual(render(explicit), explicit);
  });

  test("skips all mappings whose aliased exports resolve to one component", () => {
    const code = "const Local = () => <div/>; export {Local as Card, Local as Panel};";
    const specs = [SPEC, { exportName: "Panel", figma: "component:panel", node: "2:1" }];
    const outcome = mark(code, specs);
    assert.equal(outcome.code, null);
    assert.deepEqual(outcome.marked, []);
    assert.deepEqual(outcome.skipped.map(({ exportName }) => exportName), ["Card", "Panel"]);
    for (const { reason } of outcome.skipped) assert.match(reason, /several .* entries resolve to this component/);
    assert.deepEqual(mark(code, [...specs].reverse()), outcome);
  });

  test("skips repeated mappings of one export without inserting duplicate attributes", () => {
    const outcome = mark("export const Card = () => <div/>;", [SPEC, { ...SPEC, figma: "component:other" }]);
    assert.equal(outcome.code, null);
    assert.deepEqual(outcome.marked, []);
    assert.equal(outcome.skipped.length, 2);
  });

  test("marks every branch that renders an element, ignores null returns and nested functions", () => {
    const code = [
      "export function Card({ open }) {",
      "  const render = () => <>nested</>;",
      "  if (!open) return null;",
      "  return open === 1 ? <div>a</div> : <span>b</span>;",
      "}",
    ].join("\n");
    const outcome = mark(code, [SPEC], "Card.jsx");
    assert.deepEqual(outcome.skipped, []);
    assert.ok(outcome.code?.includes(`<div${ATTRS}>a</div>`));
    assert.ok(outcome.code?.includes(`<span${ATTRS}>b</span>`));
    assert.ok(outcome.code?.includes("<>nested</>"));
  });

  test("leaves data-figmog-node out when the node is unknown", () => {
    const outcome = mark("export const Card = () => <div/>;", [{ ...SPEC, node: null }]);
    assert.equal(outcome.code, 'export const Card = () => <div data-figmog={"component_set:abc"}/>;');
  });

  test("skips, with a reason, anything without one DOM root and changes nothing", () => {
    const cases: [string, RegExp][] = [
      ["export function Card() { return <><a/><b/></>; }", /fragment/],
      ["export function Card() { return <Other />; }", /component element/],
      ["export function Card() { return <ui.Box />; }", /component element/],
      ["export function Card() { return [<a key=\"1\"/>, <b key=\"2\"/>]; }", /not a single JSX element/],
      ["export function Card() { return cond ? <a/> : <></>; }", /fragment/],
      ["export function Card() { return null; }", /renders no element/],
      ["export const Card = 3;", /not a function component/],
      ["export function Other() { return <a/>; }", /no such export/],
      ["export function Card() { return <div data-figmog=\"x\" />; }", /explicit markers win/],
      ["export function Card( { return <a/>; }", /does not parse/],
    ];
    for (const [code, reason] of cases) {
      const outcome = mark(code);
      assert.equal(outcome.code, null, code);
      assert.deepEqual(outcome.marked, [], code);
      assert.equal(outcome.skipped.length, 1, code);
      assert.match(outcome.skipped[0]?.reason ?? "", reason, code);
    }
    assert.match(markModule("x", "Card.ts", [SPEC]).skipped[0]?.reason ?? "", /not a JSX or TSX module/);
  });

  test("marks several exports of one module and is deterministic", () => {
    const code = "export const B = () => <b/>;\nexport function A() { return <a/>; }\n";
    const specs: MarkSpec[] = [
      { exportName: "B", figma: "component:b", node: null },
      { exportName: "A", figma: "component:a", node: "1:2" },
    ];
    const first = markModule(code, "x.jsx", specs);
    assert.deepEqual(first.marked, ["A", "B"]);
    assert.equal(
      first.code,
      'export const B = () => <b data-figmog={"component:b"}/>;\n' +
        'export function A() { return <a data-figmog={"component:a"} data-figmog-node={"1:2"}/>; }\n',
    );
    assert.equal(markModule(code, "x.jsx", [...specs].reverse()).code, first.code);
  });
});
