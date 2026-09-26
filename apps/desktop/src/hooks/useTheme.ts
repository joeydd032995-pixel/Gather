import { useCallback, useEffect, useState } from "react";

export type ThemeChoice = "system" | "light" | "dark";

const KEY = "gather.theme";

function stored(): ThemeChoice {
  try {
    const value = localStorage.getItem(KEY);
    if (value === "light" || value === "dark") return value;
  } catch {
    /* storage unavailable: follow the system */
  }
  return "system";
}

function apply(choice: ThemeChoice) {
  const root = document.documentElement;
  if (choice === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", choice);
}

// Apply before the first render so the app never flashes the wrong theme.
apply(stored());

/** The appearance setting: follow the system, or force light or dark. */
export function useTheme(): [ThemeChoice, (choice: ThemeChoice) => void] {
  const [choice, setChoice] = useState<ThemeChoice>(stored);

  useEffect(() => apply(choice), [choice]);

  // Other windows (or the palette) may change it.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === KEY) setChoice(stored());
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  const update = useCallback((next: ThemeChoice) => {
    try {
      if (next === "system") localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, next);
    } catch {
      /* not persisted; still applies for this session */
    }
    setChoice(next);
  }, []);

  return [choice, update];
}
