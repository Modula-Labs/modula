import { useEffect, useState } from "react";
import { load, save } from "./storage";

export type Theme = "dark" | "light";

function apply(t: Theme) {
  document.documentElement.classList.toggle("light", t === "light");
  document.documentElement.classList.toggle("dark", t === "dark");
}

export function initialTheme(): Theme {
  const stored = load("mc.theme");
  if (stored === "light" || stored === "dark") return stored;
  return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

export function useTheme() {
  const [theme, setTheme] = useState<Theme>(initialTheme);
  useEffect(() => {
    apply(theme);
    save("mc.theme", theme);
  }, [theme]);
  return { theme, toggle: () => setTheme((t) => (t === "dark" ? "light" : "dark")) };
}

apply(initialTheme());
