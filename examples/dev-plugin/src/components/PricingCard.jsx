import { Badge } from "./Badge.jsx";
import { StarIcon } from "./StarIcon.jsx";

// One component, many instances: the automatic attributes are the same on
// every card, so verification targets one card through its data-testid.
export function PricingCard({ testId, tier, title, badge = false }) {
  return (
    <article
      data-testid={testId}
      className="flex w-72 flex-col gap-3 rounded-xl bg-white p-6 shadow-sm ring-1 ring-slate-200"
    >
      <header className="flex items-center gap-2">
        <StarIcon />
        <h2 className="text-lg font-semibold text-slate-900">{title}</h2>
        {badge ? <Badge label="Popular" /> : null}
      </header>
      <p className="text-3xl font-bold text-slate-900">{tier === "pro" ? "$12" : "$0"}</p>
    </article>
  );
}
