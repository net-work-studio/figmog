import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { markModule, type MarkSpec } from "../src/mark.js";

const SPEC: MarkSpec = { exportName: "Card", figma: "component_set:abc", node: "10:1" };
const ATTRS = ' data-figmog={"component_set:abc"} data-figmog-node={"10:1"}';

function mark(code: string, specs: MarkSpec[] = [SPEC], file = "Card.tsx") {
  return markModule(code, file, specs);
}

describe("markModule", () => {
  test("marks the root element of a function declaration, arrow and default export", () => {
    const cases: [string, string][] = [
      ["export function Card() { return <div className=\"c\">x</div>; }", `<div className="c"${ATTRS}>x</div>`],
      ["export const Card = () => (<section>x</section>);", `<section${ATTRS}>x</section>`],
      ["export const Card = (p: {a: string}) => <hr />;", `<hr ${ATTRS}/>`],
      ["export default function Card() { return <main/>; }", `<main${ATTRS}/>`],
      ["function Local() { return <i>é</i>; }\nexport { Local as Card };", `<i${ATTRS}>é</i>`],
      ["export const Card = memo(forwardRef((p, ref) => <b ref={ref} />));", `<b ref={ref} ${ATTRS}/>`],
      ["export const Card = React.memo(function Card() { return <p>x</p>; });", `<p${ATTRS}>x</p>`],
    ];
    for (const [code, expected] of cases) {
      const outcome = mark(code);
      assert.deepEqual(outcome.marked, ["Card"], code);
      assert.ok(outcome.code?.includes(expected), `${code}\n=> ${outcome.code}`);
    }
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
