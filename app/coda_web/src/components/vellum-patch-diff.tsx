import { registerCustomTheme, type ThemeRegistration } from "@pierre/diffs";
import { PatchDiff } from "@pierre/diffs/react";
import type { Theme } from "@/lib/theme";
import vellumDark from "@/themes/vellum-dark.json";
import vellumLight from "@/themes/vellum-light.json";

const VELLUM_DARK = "vellum-dark";
const VELLUM_LIGHT = "vellum-light";

registerCustomTheme(VELLUM_DARK, async () => vellumDark as ThemeRegistration);
registerCustomTheme(VELLUM_LIGHT, async () => vellumLight as ThemeRegistration);

export function VellumPatchDiff({ patch, theme }: { patch: string; theme: Theme }) {
  return (
    <PatchDiff
      patch={patch}
      disableWorkerPool
      options={{
        diffStyle: "unified",
        expandUnchanged: false,
        theme: { dark: VELLUM_DARK, light: VELLUM_LIGHT },
        themeType: theme,
      }}
    />
  );
}
