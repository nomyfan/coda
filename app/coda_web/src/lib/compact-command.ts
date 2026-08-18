/**
 * Return the optional instructions when the whole composer input is a
 * `/compact` command. Text that merely contains the token stays ordinary chat.
 */
export function parseCompactCommand(text: string): string | null {
  const match = /^\/compact(?:\s+([\s\S]*))?$/.exec(text.trim());
  return match ? (match[1]?.trim() ?? "") : null;
}
