import { Monitor, Moon, Sun } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  applyThemePreference,
  getStoredThemePreference,
  resolveTheme,
  subscribeThemeChange,
  type ThemePreference,
} from "@/lib/theme";

const OPTIONS: Array<{ value: ThemePreference; label: string }> = [
  { value: "system", label: "Auto" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

export function ThemeToggle() {
  const [preference, setPreference] = useState(getStoredThemePreference);
  const [open, setOpen] = useState(false);
  const [pendingPreference, setPendingPreference] = useState<ThemePreference | null>(null);
  const resolved = resolveTheme(preference);
  const Icon = preference === "system" ? Monitor : resolved === "dark" ? Moon : Sun;

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");

    function syncFromStorage() {
      setPreference(getStoredThemePreference());
    }

    function syncSystemTheme() {
      if (getStoredThemePreference() === "system") {
        applyThemePreference("system");
      }
      syncFromStorage();
    }

    const unsubscribe = subscribeThemeChange(syncFromStorage);
    media.addEventListener("change", syncSystemTheme);

    return () => {
      unsubscribe();
      media.removeEventListener("change", syncSystemTheme);
    };
  }, []);

  useEffect(() => {
    if (open || pendingPreference === null) {
      return;
    }

    const timer = window.setTimeout(() => {
      setPreference(pendingPreference);
      applyThemePreference(pendingPreference);
      setPendingPreference(null);
    }, 300);

    return () => window.clearTimeout(timer);
  }, [open, pendingPreference]);

  function setTheme(nextPreference: string) {
    const next = nextPreference as ThemePreference;
    setPendingPreference(next);
    setOpen(false);
  }

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" className="size-8 shrink-0" title="Theme">
          <Icon className="size-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuRadioGroup value={preference} onValueChange={setTheme}>
          {OPTIONS.map((option) => (
            <DropdownMenuRadioItem key={option.value} value={option.value}>
              {option.label}
            </DropdownMenuRadioItem>
          ))}
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
