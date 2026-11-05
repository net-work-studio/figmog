import { Badge } from "./Badge.jsx";
import { Card } from "./Card.jsx";

// Classic JSX runtime without React: the tests only check the transformed
// modules and the served HTML, never render them.
globalThis.h = () => null;
export const view = [Card({ title: "Plan" }), Badge({ label: "New" })];
