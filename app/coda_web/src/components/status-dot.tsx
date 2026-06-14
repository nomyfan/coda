import { cn } from "@/lib/utils";

export type DotTone = "online" | "offline" | "busy";

export type DotMotion = "static" | "breathe" | "ping";

/** A status dot wrapped in a soft halo. `motion` animates the halo: `breathe`
 * for an in-progress (loading) feel, `ping` for an attention-seeking pulse
 * (e.g. awaiting approval); `static` leaves it still. */
export function StatusDot({
  tone,
  motion = "static",
  title,
  className,
}: {
  tone: DotTone;
  motion?: DotMotion;
  title?: string;
  className?: string;
}) {
  const color =
    tone === "online" ? "bg-emerald-500" : tone === "busy" ? "bg-amber-500" : "bg-rose-500";
  return (
    <span
      className={cn("relative flex size-3 shrink-0 items-center justify-center", className)}
      title={title}
    >
      <span
        aria-hidden
        className={cn(
          "absolute inline-flex size-full rounded-full opacity-30",
          color,
          motion === "breathe" && "animate-breathe",
          motion === "ping" && "animate-ping-soft",
        )}
      />
      <span className={cn("relative inline-flex size-1.5 rounded-full", color)} />
    </span>
  );
}
