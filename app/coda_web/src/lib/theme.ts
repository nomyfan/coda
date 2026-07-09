export type Theme = "light" | "dark";
export type ThemePreference = "system" | Theme;

const STORAGE_KEY = "coda-theme";
const THEME_CHANGE_EVENT = "coda-theme-change";

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

/* Remove the pre-CSS seed painted by the index.html inline script so html/body
 * fall through to the stylesheet's bg-background. */
function clearSeedBackground() {
  document.getElementById("theme-seed")?.remove();
  document.documentElement.style.removeProperty("background-color");
}

/* Mirror the effective page background (--background via the stylesheet) into
 * the theme-color meta, so browser chrome follows index.css without a second
 * copy of the color anywhere in code. */
function syncThemeColorMeta() {
  const color = getComputedStyle(document.body).backgroundColor;
  document.querySelectorAll('meta[name="theme-color"]').forEach((meta) => meta.remove());
  const meta = document.createElement("meta");
  meta.name = "theme-color";
  meta.content = color;
  document.head.append(meta);
}

export function applyThemePreference(preference: ThemePreference): Theme {
  const theme = resolveTheme(preference);
  document.documentElement.classList.toggle("dark", theme === "dark");
  document.documentElement.style.colorScheme = theme;
  clearSeedBackground();
  syncThemeColorMeta();
  localStorage.setItem(STORAGE_KEY, preference);
  window.dispatchEvent(new CustomEvent(THEME_CHANGE_EVENT));
  return theme;
}

export function subscribeThemeChange(listener: () => void) {
  window.addEventListener(THEME_CHANGE_EVENT, listener);
  return () => window.removeEventListener(THEME_CHANGE_EVENT, listener);
}
