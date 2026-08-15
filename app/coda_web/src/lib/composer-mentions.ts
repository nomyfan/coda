/**
 * The composer's `@` / `/` pickers, minus the rendering: what the caret is
 * sitting in, what accepting an item does to the text, and how a query ranks the
 * items that are filtered client-side.
 *
 * A mention is normally a whitespace-delimited token that *starts* with the
 * trigger character, so `@` in an email address and `/` inside a path (`@src/`)
 * never open a menu of their own. A path that contains whitespace is quoted
 * (`@"my notes/todo.md"`) so it can still be one token.
 */

/** `@` picks a workspace file; `/` picks a skill. */
export type MentionKind = "file" | "slash";

export type MentionTrigger = {
  kind: MentionKind;
  /** Index of the `@` / `/` that opened the menu. */
  start: number;
  /** Index just past the query — the caret. Accepting replaces `[start, end)`. */
  end: number;
  /** What has been typed after the trigger character, unquoted. */
  query: string;
};

export type MentionItemKind = "file" | "directory" | "skill";

export type MentionItem = {
  kind: MentionItemKind;
  /** Inserted after the trigger character; also the item's identity in the list. */
  value: string;
  /** Second line in the menu: a skill's description, a file's parent directory. */
  detail?: string;
};

/** Past this a "query" is prose that happens to follow a `@`, not a search. */
const MAX_QUERY_LENGTH = 100;

function isTokenBoundary(char: string | undefined): boolean {
  return char === undefined || /\s/.test(char);
}

function hasWhitespace(value: string): boolean {
  return /\s/.test(value);
}

/**
 * The start of an unclosed quoted mention (`@"my notes/`) ending at `caret`, or
 * `null` when the caret is not inside one. Quoting is what lets a path with
 * spaces stay a single token, so it has to be recognised before the ordinary
 * whitespace scan — which would stop at the first space inside the quotes.
 */
function quotedStart(text: string, caret: number): number | null {
  for (let index = caret - 1; index >= 0; index -= 1) {
    const char = text[index];
    // A mention never spans lines, so this bounds the scan.
    if (char === "\n") {
      return null;
    }
    if (char !== '"') {
      continue;
    }
    // The first quote back is either the one that opened this mention, or a
    // closing quote — in which case the caret is past the mention entirely.
    const prefix = text[index - 1];
    const opensMention = (prefix === "@" || prefix === "/") && isTokenBoundary(text[index - 2]);
    return opensMention ? index - 1 : null;
  }
  return null;
}

/**
 * The mention the caret is currently typing, or `null` when it isn't in one.
 *
 * The caret has to sit at the *end* of the token: completing from the middle of
 * one would silently drop everything after the caret, so a caret parked back in
 * a word (someone fixing a typo) leaves the menu closed.
 */
export function detectTrigger(text: string, caret: number): MentionTrigger | null {
  if (caret < 0 || caret > text.length || !isTokenBoundary(text[caret])) {
    return null;
  }

  let start = quotedStart(text, caret);
  if (start === null) {
    start = caret;
    while (start > 0 && !isTokenBoundary(text[start - 1])) {
      start -= 1;
    }
  }

  const token = text.slice(start, caret);
  const kind: MentionKind | null = token.startsWith("@")
    ? "file"
    : token.startsWith("/")
      ? "slash"
      : null;
  if (kind === null) {
    return null;
  }

  // Drop the quotes a whitespace-bearing path is wrapped in; the query is the
  // path itself either way.
  const query = token[1] === '"' ? token.slice(2).replace(/"$/, "") : token.slice(1);
  if (query.length > MAX_QUERY_LENGTH) {
    return null;
  }

  return { kind, start, end: caret, query };
}

/**
 * Splice `item` into `text` in place of the trigger's token, answering with the
 * caret position that follows it.
 *
 * A directory keeps the menu going — it ends in `/` with no trailing space, so
 * the next keystroke searches inside it. Everything else is a finished choice
 * and gets the space that closes the menu, unless the text already has one
 * there.
 *
 * A value containing whitespace is quoted, or the token would end at its first
 * space and the mention would read as a fragment plus prose. A directory's
 * quote is left open so the menu can carry on inside it.
 */
export function applyMention(
  text: string,
  trigger: MentionTrigger,
  item: MentionItem,
): { text: string; caret: number } {
  const prefix = trigger.kind === "file" ? "@" : "/";
  const value =
    item.kind === "directory" && !item.value.endsWith("/") ? `${item.value}/` : item.value;
  const isDirectory = item.kind === "directory";
  const quote = hasWhitespace(value) ? '"' : "";
  const token = isDirectory ? `${prefix}${quote}${value}` : `${prefix}${quote}${value}${quote}`;

  const followedBySpace = hasWhitespace(text[trigger.end] ?? "");
  const insertion = isDirectory || followedBySpace ? token : `${token} `;
  return {
    text: text.slice(0, trigger.start) + insertion + text.slice(trigger.end),
    // Step over the space that was already there, so typing carries on after
    // the mention rather than gluing onto it.
    caret: trigger.start + insertion.length + (!isDirectory && followedBySpace ? 1 : 0),
  };
}

const MATCH_SCORE = 8;
const BOUNDARY_BONUS = 12;
const CONSECUTIVE_BONUS = 10;
const SUBSTRING_BONUS = 24;

function isWordBoundary(char: string): boolean {
  return char === "/" || char === "_" || char === "-" || char === "." || char === " ";
}

/**
 * How well `candidate` answers `query`, or `null` when the query's characters
 * don't appear in it in order: runs of adjacent characters and matches that
 * start a word score highest, and a verbatim substring beats a scattered
 * subsequence.
 *
 * This ranks the `/` menu only, and is deliberately its own thing — not a port
 * of the server's `fuzzy_score` (`app/coda_server/src/files.rs`). That one
 * ranks *paths*, so it weighs a basename against the directories leading to it
 * and penalises length; a skill name has neither. The two share three constant
 * names and nothing else, and neither is obliged to follow the other.
 */
export function fuzzyScore(candidate: string, query: string): number | null {
  if (query === "") {
    return 0;
  }

  const haystack = candidate.toLowerCase();
  const needle = query.toLowerCase();
  let score = 0;
  let next = 0;
  let previousMatched = false;

  for (let index = 0; index < haystack.length && next < needle.length; index += 1) {
    if (haystack[index] !== needle[next]) {
      previousMatched = false;
      continue;
    }
    score += MATCH_SCORE;
    if (previousMatched) {
      score += CONSECUTIVE_BONUS;
    }
    if (index === 0 || isWordBoundary(haystack[index - 1])) {
      score += BOUNDARY_BONUS;
    }
    next += 1;
    previousMatched = true;
  }

  if (next < needle.length) {
    return null;
  }
  return haystack.includes(needle) ? score + SUBSTRING_BONUS : score;
}

/**
 * The items `query` matches, best first. Ties keep the order they came in, so
 * the caller's own ordering (skills alphabetical) survives.
 */
export function rankMentionItems(items: MentionItem[], query: string): MentionItem[] {
  if (query === "") {
    return items;
  }
  return items
    .map((item) => ({ item, score: fuzzyScore(item.value, query) }))
    .filter((scored): scored is { item: MentionItem; score: number } => scored.score !== null)
    .sort((a, b) => b.score - a.score)
    .map((scored) => scored.item);
}

/** What "nothing matched" reads as for a trigger. */
export function emptyMentionLabel(trigger: MentionTrigger): string {
  return trigger.kind === "file" ? "No matching files" : "No matching skills";
}

/** The file name (or directory name) a path ends in. */
export function mentionName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || trimmed;
}

/** The directories leading to `path`, or `""` when it sits at the workspace root. */
export function mentionParent(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut === -1 ? "" : trimmed.slice(0, cut);
}
