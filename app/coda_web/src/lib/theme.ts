export type Theme = "light" | "dark";
export type ThemePreference = "system" | Theme;

const STORAGE_KEY = "coda-theme";
const THEME_CHANGE_EVENT = "coda-theme-change";
const THEME_COLORS: Record<Theme, string> = {
  light: "#fcfbf9",
  dark: "#2a2725",
};

export function getStoredThemePreference(): ThemePreference {
  const saved = localStorage.getItem(STORAGE_KEY);
  if (saved === "system" || saved === "light" || saved === "dark") {
    return saved;
  }
  return "system";
}

export function resolveTheme(preference: ThemePreference): Theme {
  if (preference === "system") {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return preference;
}

function addThemeColorMeta(content: string, media?: string) {
  const meta = document.createElement("meta");
  meta.name = "theme-color";
  meta.content = content;
  if (media) {
    meta.media = media;
  }
  document.head.append(meta);
}

function applyThemeColor(preference: ThemePreference, theme: Theme) {
  document.querySelectorAll('meta[name="theme-color"]').forEach((meta) => meta.remove());

  if (preference === "system") {
    addThemeColorMeta(THEME_COLORS.light, "(prefers-color-scheme: light)");
    addThemeColorMeta(THEME_COLORS.dark, "(prefers-color-scheme: dark)");
    return;
  }

  addThemeColorMeta(THEME_COLORS[theme]);
}

function applyViewportBackground(theme: Theme) {
  const color = THEME_COLORS[theme];
  document.documentElement.style.setProperty("background-color", color, "important");
  document.body.style.setProperty("background-color", color, "important");
}

export function applyThemePreference(preference: ThemePreference): Theme {
  const theme = resolveTheme(preference);
  document.documentElement.classList.toggle("dark", theme === "dark");
  document.documentElement.style.colorScheme = theme;
  applyViewportBackground(theme);
  applyThemeColor(preference, theme);
  localStorage.setItem(STORAGE_KEY, preference);
  window.dispatchEvent(new CustomEvent(THEME_CHANGE_EVENT));
  return theme;
}

export function subscribeThemeChange(listener: () => void) {
  window.addEventListener(THEME_CHANGE_EVENT, listener);
  return () => window.removeEventListener(THEME_CHANGE_EVENT, listener);
}
