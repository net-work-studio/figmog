import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { Card } from "./Card.jsx";
import "./index.css";

const root = createRoot(document.getElementById("root"));
// Render synchronously so the card exists when the page's load event fires.
flushSync(() => {
  root.render(
    <main className="p-[40px]">
      <Card />
    </main>,
  );
});
