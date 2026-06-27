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

function pad2(value: number): string {
  return value.toString().padStart(2, "0");
}

function sameLocalDate(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/** Wall-clock time of a message, including date when it is outside today. */
export function formatClockTime(iso: string | null | undefined): string | undefined {
  const ms = parseTime(iso);
  if (ms === undefined) {
    return undefined;
  }
  const date = new Date(ms);
  const time = `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  const today = new Date();

  if (sameLocalDate(date, today)) {
    return time;
  }

  const monthDay = `${pad2(date.getMonth() + 1)}/${pad2(date.getDate())}`;
  if (date.getFullYear() === today.getFullYear()) {
    return `${monthDay} ${time}`;
  }

  return `${date.getFullYear()}/${monthDay} ${time}`;
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
