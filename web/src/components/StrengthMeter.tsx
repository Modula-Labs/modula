/** Three-segment bar for a `strength()` score (lib/password). */
export function StrengthMeter({ score }: { score: 0 | 1 | 2 | 3 }) {
  return (
    <div className="-mt-2 grid grid-cols-3 gap-1" aria-hidden>
      {[1, 2, 3].map((i) => (
        <div key={i} className={`h-1 rounded-full ${score >= i ? (score === 3 ? "bg-success" : score === 2 ? "bg-primary" : "bg-warning") : "bg-accent"}`} />
      ))}
    </div>
  );
}
