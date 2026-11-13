import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import { PricingCard } from "./components/PricingCard.jsx";
import "./index.css";

const root = createRoot(document.getElementById("root"));
// Render synchronously so the cards exist when the page's load event fires.
flushSync(() => {
  root.render(
    <main className="flex flex-wrap gap-6 bg-slate-100 p-10">
      <PricingCard testId="pricing-pro" tier="pro" title="Pro" badge />
      <PricingCard testId="pricing-free" tier="free" title="Free" />
    </main>,
  );
});
