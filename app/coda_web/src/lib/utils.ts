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
  const date = new Date(ms);
  const hour = date.getHours().toString().padStart(2, "0");
  const minute = date.getMinutes().toString().padStart(2, "0");
  return `${hour}:${minute}`;
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
  const totalSeconds = Math.round(ms / 1000);
  if (totalSeconds >= 60) {
    const minutes = Math.floor(totalSeconds / 60);
    const remainder = totalSeconds % 60;
    return `${minutes}m ${remainder}s`;
  }
  const seconds = ms / 1000;
  return `${seconds.toFixed(1)}s`;
}
