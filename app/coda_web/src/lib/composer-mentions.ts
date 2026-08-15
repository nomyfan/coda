/**
 * The composer's `@` / `/` pickers, minus the rendering: what the caret is
 * sitting in, what accepting an item does to the text, and how a query ranks the
 * items that are filtered client-side.
 *
 * A mention is always a whitespace-delimited token that *starts* with the
 * trigger character, so `@` in an email address and `/` inside a path (`@src/`)
 * never open a menu of their own.
 */

/** `@` picks a workspace file; `/` picks a skill or a built-in command. */
export type MentionKind = "file" | "slash";

export type MentionTrigger = {
  kind: MentionKind;
  /** Index of the `@` / `/` that opened the menu. */
  start: number;
  /** Index just past the query — the caret. Accepting replaces `[start, end)`. */
  end: number;
  /** What has been typed after the trigger character. */
  query: string;
  /**
   * Only whitespace precedes the trigger, so the message *is* this token. Built-in
   * commands are offered here and nowhere else: a command is the whole
   * instruction, while a skill can be named in the middle of a sentence.
   */
  atMessageStart: boolean;
};

export type MentionItemKind = "file" | "directory" | "skill" | "command";

export type MentionItem = {
  kind: MentionItemKind;
  /** Inserted after the trigger character; also the item's identity in the list. */
  value: string;
  /** Second line in the menu: a skill's description, a file's parent directory. */
  detail?: string;
};

/**
 * A built-in slash command. None exist yet — the picker lands first, and each
 * command arrives as an entry here paired with the behaviour it triggers.
 */
export type BuiltinCommand = {
  name: string;
  description: string;
};

export const BUILTIN_COMMANDS: BuiltinCommand[] = [];

/** Past this a "query" is prose that happens to follow a `@`, not a search. */
const MAX_QUERY_LENGTH = 100;

function isTokenBoundary(char: string | undefined): boolean {
  return char === undefined || /\s/.test(char);
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

  let start = caret;
  while (start > 0 && !isTokenBoundary(text[start - 1])) {
    start -= 1;
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

  const query = token.slice(1);
  if (query.length > MAX_QUERY_LENGTH) {
    return null;
  }

  return {
    kind,
    start,
    end: caret,
    query,
    atMessageStart: text.slice(0, start).trim() === "",
  };
}

/**
 * Splice `item` into `text` in place of the trigger's token, answering with the
 * caret position that follows it.
 *
 * A directory keeps the menu going — it ends in `/` with no trailing space, so
 * the next keystroke searches inside it. Everything else is a finished choice
 * and gets the space that closes the menu.
 */
export function applyMention(
  text: string,
  trigger: MentionTrigger,
  item: MentionItem,
): { text: string; caret: number } {
  const prefix = trigger.kind === "file" ? "@" : "/";
  const value =
    item.kind === "directory" && !item.value.endsWith("/") ? `${item.value}/` : item.value;
  const insertion = item.kind === "directory" ? `${prefix}${value}` : `${prefix}${value} `;
  return {
    text: text.slice(0, trigger.start) + insertion + text.slice(trigger.end),
    caret: trigger.start + insertion.length,
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
 * don't appear in it in order. Mirrors the server's file ranking so the two
 * menus feel like one: runs of adjacent characters and matches that start a word
 * score highest, and a verbatim substring beats a scattered subsequence.
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
 * The items `query` matches, best first. Ties keep the order they came in, so a
 * caller's own ordering (skills alphabetical, commands as registered) survives.
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

/** What "nothing matched" reads as for a trigger — commands are only in play at
 * the start of a message, and only once some exist. */
export function emptyMentionLabel(trigger: MentionTrigger): string {
  if (trigger.kind === "file") {
    return "No matching files";
  }
  return trigger.atMessageStart && BUILTIN_COMMANDS.length > 0
    ? "No matching commands or skills"
    : "No matching skills";
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
