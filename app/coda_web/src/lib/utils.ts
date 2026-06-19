import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

function parseTime(iso: string | null | undefined): number | undefined {
  if (!iso) {
    return undefined;
  }
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? undefined : ms;
}

/** Wall-clock time of a message as `HH:MM`, or `undefined` when absent/invalid. */
export function formatClockTime(iso: string | null | undefined): string | undefined {
  const ms = parseTime(iso);
  if (ms === undefined) {
    return undefined;
  }
  return new Date(ms).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

/**
 * Human-readable elapsed time between two RFC 3339 timestamps: `850ms`, `3.2s`,
 * or `1m 5s`. Returns `undefined` when either bound is missing or invalid.
 */
export function formatDuration(
  start: string | null | undefined,
  end: string | null | undefined,
): string | undefined {
  const from = parseTime(start);
  const to = parseTime(end);
  if (from === undefined || to === undefined) {
    return undefined;
  }
  const ms = Math.max(0, to - from);
  if (ms < 1000) {
    return `${Math.round(ms)}ms`;
  }
  const seconds = ms / 1000;
  if (seconds < 60) {
    return `${seconds.toFixed(1)}s`;
  }
  const minutes = Math.floor(seconds / 60);
  const remainder = Math.round(seconds - minutes * 60);
  return `${minutes}m ${remainder}s`;
}
