// A fragment root: there is no single element to mark, so the plugin skips
// Badge and says so on the dev server's console.
export function Badge({ label }) {
  return (
    <>
      <span className="rounded-full bg-indigo-600 px-2 py-0.5 text-xs font-medium text-white">{label}</span>
      <span className="sr-only">(recommended)</span>
    </>
  );
}
