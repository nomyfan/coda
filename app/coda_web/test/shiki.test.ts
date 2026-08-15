import { describe, expect, it } from "vitest";
import { highlight, loadLanguage, resolveLanguage } from "@/lib/shiki";

describe("language resolution", () => {
  it("maps common fence aliases onto shipped grammars", () => {
    expect(resolveLanguage("js")).toBe("javascript");
    expect(resolveLanguage("TS")).toBe("typescript");
    expect(resolveLanguage("sh")).toBe("bash");
    expect(resolveLanguage("rs")).toBe("rust");
  });

  it("declines fences with no language and grammars we don't bundle", () => {
    expect(resolveLanguage(undefined)).toBeNull();
    expect(resolveLanguage("")).toBeNull();
    expect(resolveLanguage("brainfuck")).toBeNull();
  });
});

describe("highlighting", () => {
  it("returns null until the grammar is loaded, so callers render plain text", () => {
    expect(highlight("const a = 1", "javascript")).toBeNull();
  });

  it("emits bare spans carrying both theme colors once the grammar is in", async () => {
    await loadLanguage("javascript");
    const html = highlight("const a = 1", "javascript");

    // The caller supplies its own <pre>/<code>; shiki contributes colors only.
    expect(html).not.toContain("<pre");
    expect(html).not.toContain("<code");
    // Light inline, dark as the custom property index.css flips to.
    expect(html).toContain("--shiki-dark:");
    expect(html).toMatch(/<span[^>]*>const<\/span>/);
  });
});
