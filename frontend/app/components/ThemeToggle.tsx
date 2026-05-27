"use client";

import { useTheme } from "../providers/theme-provider";
import type { Theme } from "../providers/theme-provider";

const THEME_META: Record<Theme, { label: string; ariaLabel: string }> = {
  light:  { label: "Light",  ariaLabel: "Switch to Dark mode" },
  dark:   { label: "Dark",   ariaLabel: "Switch to System mode" },
  system: { label: "System", ariaLabel: "Switch to Light mode" },
};

export function ThemeToggle() {
  const { theme, toggleTheme } = useTheme();
  const { label, ariaLabel } = THEME_META[theme] ?? THEME_META.system;

  return (
    <button
      onClick={toggleTheme}
      className="rounded-lg border border-zinc-300 dark:border-zinc-600 px-3 py-2 text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800 focus:outline-none focus-visible:ring-2 focus-visible:ring-zinc-400 focus-visible:ring-offset-2"
      aria-label={ariaLabel}
      title={ariaLabel}
    >
      {label}
    </button>
  );
}
