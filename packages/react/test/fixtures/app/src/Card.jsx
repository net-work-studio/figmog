export function Card({ title }) {
  return (
    <section className="card" data-testid="card">
      <h2>{title}</h2>
      <Star />
    </section>
  );
}

export const Star = () => <svg width="8" height="8" />;
