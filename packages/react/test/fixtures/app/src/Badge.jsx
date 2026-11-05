// A fragment root: the plugin cannot pick one element, so it skips Badge.
export function Badge({ label }) {
  return (
    <>
      <span>{label}</span>
      <span>!</span>
    </>
  );
}
