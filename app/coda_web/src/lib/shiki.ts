import { useEffect, useState } from "react";
import { createHighlighterCore, type HighlighterCore, type ThemeRegistration } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import vellumDark from "@/themes/vellum-dark.json";
import vellumLight from "@/themes/vellum-light.json";

/* Both themes are baked into every highlight pass: shiki emits the light color
 * inline and the dark one as a `--shiki-dark` custom property, and index.css
 * flips between them under `.dark`. That way a theme switch costs nothing —
 * no re-highlight, no theme plumbing through the component tree. */
const LIGHT = "vellum-light";
const DARK = "vellum-dark";

/* Explicit loaders rather than a template-literal `import()`: a dynamic
 * specifier would make Vite bundle all ~300 shiki grammars. Each entry here is
 * its own lazily-fetched chunk. */
const LANGUAGES: Record<string, () => Promise<unknown>> = {
  bash: () => import("shiki/langs/bash.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  csharp: () => import("shiki/langs/csharp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  docker: () => import("shiki/langs/docker.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  graphql: () => import("shiki/langs/graphql.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  ini: () => import("shiki/langs/ini.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsonc: () => import("shiki/langs/jsonc.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  kotlin: () => import("shiki/langs/kotlin.mjs"),
  lua: () => import("shiki/langs/lua.mjs"),
  make: () => import("shiki/langs/make.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  nix: () => import("shiki/langs/nix.mjs"),
  php: () => import("shiki/langs/php.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  ruby: () => import("shiki/langs/ruby.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  typescript: () => import("shiki/langs/typescript.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  zig: () => import("shiki/langs/zig.mjs"),
};

const ALIASES: Record<string, string> = {
  "c++": "cpp",
  "c#": "csharp",
  cs: "csharp",
  dockerfile: "docker",
  htm: "html",
  js: "javascript",
  jsonl: "json",
  kt: "kotlin",
  makefile: "make",
  md: "markdown",
  mjs: "javascript",
  cjs: "javascript",
  objc: "c",
  patch: "diff",
  py: "python",
  rb: "ruby",
  rs: "rust",
  sh: "bash",
  shell: "bash",
  svg: "xml",
  ts: "typescript",
  yml: "yaml",
  zsh: "bash",
};

/** Canonical shiki language id, or null when we don't ship a grammar for it. */
export function resolveLanguage(lang: string | undefined): string | null {
  if (!lang) return null;
  const key = lang.toLowerCase();
  const canonical = ALIASES[key] ?? key;
  return canonical in LANGUAGES ? canonical : null;
}

let highlighterPromise: Promise<HighlighterCore> | null = null;
let highlighter: HighlighterCore | null = null;
const loading = new Map<string, Promise<void>>();
/** Languages whose grammar is loaded into the live highlighter. */
const ready = new Set<string>();

function getHighlighter() {
  highlighterPromise ??= createHighlighterCore({
    themes: [vellumLight as ThemeRegistration, vellumDark as ThemeRegistration],
    langs: [],
    // The JS engine keeps us off the oniguruma wasm blob; `forgiving` skips the
    // few regexes it can't translate instead of failing the whole grammar.
    engine: createJavaScriptRegexEngine({ forgiving: true }),
  }).then((created) => {
    highlighter = created;
    return created;
  });
  return highlighterPromise;
}

/** Bring a grammar into the shared highlighter; repeat calls share one load. */
export function loadLanguage(lang: string) {
  let pending = loading.get(lang);
  if (!pending) {
    pending = (async () => {
      const [core, grammar] = await Promise.all([getHighlighter(), LANGUAGES[lang]()]);
      await core.loadLanguage(grammar as never);
      ready.add(lang);
    })();
    loading.set(lang, pending);
  }
  return pending;
}

/**
 * Highlight `code` to bare `<span>` markup — no `<pre>`/`<code>` wrapper, so
 * the caller keeps full control of block styling and only the token colors
 * come from shiki. Returns null until the grammar has loaded.
 */
export function highlight(code: string, lang: string): string | null {
  if (!highlighter || !ready.has(lang)) return null;
  return highlighter.codeToHtml(code, {
    lang,
    themes: { light: LIGHT, dark: DARK },
    // Both colors as custom properties, none as an inline `color`: an inline
    // color would outrank the stylesheet's dark rule and pin every block to
    // the light palette.
    defaultColor: false,
    cssVariablePrefix: "--shiki-",
    structure: "inline",
  });
}

/**
 * Highlighted markup for a fenced block, or null while the grammar loads (or
 * for languages we don't bundle) — render the plain text in that case.
 */
export function useHighlighted(code: string, lang: string | undefined): string | null {
  const resolved = resolveLanguage(lang);
  const [, forceRender] = useState(0);

  useEffect(() => {
    if (!resolved || ready.has(resolved)) return;
    let live = true;
    loadLanguage(resolved)
      .then(() => {
        if (live) forceRender((n) => n + 1);
      })
      .catch(() => {
        /* Fall back to plain text; a missing grammar isn't worth surfacing. */
      });
    return () => {
      live = false;
    };
  }, [resolved]);

  return resolved ? highlight(code, resolved) : null;
}
