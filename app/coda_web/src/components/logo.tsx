/**
 * The Coda mark — the letter `d`: a bowl plus a stem tangent to its right edge.
 *
 * The stem stops at the bowl's center height — the point where it runs tangent
 * to the bowl — rather than carrying on to the baseline, so bowl center and stem
 * end sit on one line. Both strokes are 10 wide, which also puts the stem's outer
 * edge exactly on the bowl's rightmost point (32 + 15 + 5 = 52) and makes the
 * mark a clean 40×48 centered in the 64 canvas.
 *
 * The pale bowl carries the letterform and the darker stem carries the accent;
 * `mono` collapses both onto `currentColor` for small sizes and tinted contexts.
 */
export function CodaMark({ className, mono = false }: { className?: string; mono?: boolean }) {
  return (
    <svg
      viewBox="0 0 64 64"
      role="img"
      aria-label="Coda"
      className={className}
      xmlns="http://www.w3.org/2000/svg"
    >
      <circle
        cx="32"
        cy="36"
        r="15"
        fill="none"
        stroke={mono ? "currentColor" : "var(--brand-pale)"}
        strokeWidth="10"
      />
      <path d="M47 36V8" stroke={mono ? "currentColor" : "var(--primary)"} strokeWidth="10" />
    </svg>
  );
}
