import { expect, test } from "vitest";

import {
  applyMention,
  detectTrigger,
  emptyMentionLabel,
  fuzzyScore,
  type MentionItem,
  mentionName,
  mentionParent,
  rankMentionItems,
} from "../src/lib/composer-mentions.ts";

/** Detect at the end of `text`, which is where a caret sits while typing. */
function atEnd(text: string) {
  return detectTrigger(text, text.length);
}

// --- detectTrigger -----------------------------------------------------------

test("a bare @ opens the file picker with an empty query", () => {
  expect(atEnd("@")).toEqual({
    kind: "file",
    start: 0,
    end: 1,
    query: "",
    atMessageStart: true,
  });
});

test("the query is everything typed after the trigger", () => {
  expect(atEnd("look at @src/components/comp")).toMatchObject({
    kind: "file",
    query: "src/components/comp",
    start: 8,
  });
});

test("a slash inside a path does not start a mention of its own", () => {
  // The token starts with `@`, so the `/` in it belongs to the path.
  expect(atEnd("@src/comp")).toMatchObject({ kind: "file", query: "src/comp" });
});

test("a trigger has to start its token, so an email address is left alone", () => {
  expect(atEnd("mail bob@example.com")).toBeNull();
});

test("a slash names a skill anywhere, but only opens the message at its start", () => {
  expect(atEnd("/rev")).toMatchObject({ kind: "slash", query: "rev", atMessageStart: true });
  expect(atEnd("  /rev")).toMatchObject({ atMessageStart: true });
  expect(atEnd("please run /rev")).toMatchObject({ kind: "slash", atMessageStart: false });
  // A second line is not the start of the message.
  expect(atEnd("context\n/rev")).toMatchObject({ kind: "slash", atMessageStart: false });
});

test("no menu opens from the middle of a token", () => {
  // Caret between `com` and `poser`: completing here would drop the tail.
  expect(detectTrigger("@composer", 4)).toBeNull();
  // Caret at the end of the token but with more text after it is fine.
  expect(detectTrigger("@comp and more", 5)).toMatchObject({ query: "comp" });
});

test("the token ends at whitespace", () => {
  expect(atEnd("@src/main.rs ")).toBeNull();
  expect(atEnd("@src/main.rs and")).toBeNull();
});

test("prose that happens to follow a trigger is not a query", () => {
  expect(atEnd(`@${"x".repeat(101)}`)).toBeNull();
  expect(atEnd(`@${"x".repeat(100)}`)).toMatchObject({ kind: "file" });
});

// --- applyMention ------------------------------------------------------------

const trigger = (text: string) => {
  const found = atEnd(text);
  if (!found) throw new Error(`no trigger in ${JSON.stringify(text)}`);
  return found;
};

test("accepting a file replaces the token and leaves the caret past a space", () => {
  const text = "look at @comp";
  const applied = applyMention(text, trigger(text), {
    kind: "file",
    value: "src/components/composer.tsx",
  });

  expect(applied.text).toBe("look at @src/components/composer.tsx ");
  expect(applied.caret).toBe(applied.text.length);
});

test("accepting a directory keeps the query going", () => {
  const text = "@comp";
  const applied = applyMention(text, trigger(text), { kind: "directory", value: "src/components" });

  // No trailing space: the next keystroke searches inside the directory.
  expect(applied.text).toBe("@src/components/");
  expect(applied.caret).toBe(applied.text.length);
  expect(detectTrigger(applied.text, applied.caret)).toMatchObject({
    kind: "file",
    query: "src/components/",
  });
});

test("accepting a skill inserts its name and closes the menu", () => {
  const text = "please /rev";
  const applied = applyMention(text, trigger(text), { kind: "skill", value: "code-review" });

  expect(applied.text).toBe("please /code-review ");
  expect(detectTrigger(applied.text, applied.caret)).toBeNull();
});

test("text after the token survives the insertion", () => {
  const text = "see @comp for the details";
  const found = detectTrigger(text, 9);
  expect(found).toMatchObject({ query: "comp" });
  const applied = applyMention(text, found!, { kind: "file", value: "composer.tsx" });

  expect(applied.text).toBe("see @composer.tsx  for the details");
  expect(applied.caret).toBe("see @composer.tsx ".length);
});

// --- ranking -----------------------------------------------------------------

test("a query matches its characters in order and nothing else", () => {
  expect(fuzzyScore("code-review", "crev")).not.toBeNull();
  expect(fuzzyScore("code-review", "verc")).toBeNull();
  expect(fuzzyScore("code-review", "codex")).toBeNull();
  expect(fuzzyScore("code-review", "")).toBe(0);
});

test("a verbatim match outranks a scattered one", () => {
  const verbatim = fuzzyScore("code-review", "review");
  // `r-e…v-i-e-w` in order, but never as one run.
  const scattered = fuzzyScore("release-preview", "review");
  expect(scattered).not.toBeNull();
  expect(verbatim!).toBeGreaterThan(scattered!);
});

test("ranking drops non-matches and keeps input order for ties", () => {
  const items: MentionItem[] = [
    { kind: "command", value: "compact" },
    { kind: "skill", value: "code-review" },
    { kind: "skill", value: "deploy" },
  ];

  expect(rankMentionItems(items, "co").map((item) => item.value)).toEqual([
    "compact",
    "code-review",
  ]);
  expect(rankMentionItems(items, "").map((item) => item.value)).toEqual([
    "compact",
    "code-review",
    "deploy",
  ]);
});

// --- display helpers ---------------------------------------------------------

test("a path shows as a name and the directories leading to it", () => {
  expect(mentionName("app/coda_web/src/composer.tsx")).toBe("composer.tsx");
  expect(mentionParent("app/coda_web/src/composer.tsx")).toBe("app/coda_web/src");
  expect(mentionName("README.md")).toBe("README.md");
  expect(mentionParent("README.md")).toBe("");
  expect(mentionName("src/components/")).toBe("components");
});

test("an empty menu says what it was looking for", () => {
  expect(emptyMentionLabel(trigger("@comp"))).toBe("No matching files");
  // No built-in commands exist yet, so even at the start of a message the `/`
  // menu is skills only.
  expect(emptyMentionLabel(trigger("/rev"))).toBe("No matching skills");
});
