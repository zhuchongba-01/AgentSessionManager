import React, {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { safeLocalStorageGet, safeLocalStorageSet } from "@/lib/utils";

type Theme = "light" | "dark" | "system";

interface ThemeProviderProps {
  children: React.ReactNode;
  defaultTheme?: Theme;
  storageKey?: string;
}

interface ThemeContextValue {
  theme: Theme;
  resolvedTheme: "light" | "dark";
  setTheme: (theme: Theme) => void;
}

const ThemeProviderContext = createContext<ThemeContextValue | undefined>(
  undefined,
);

export function ThemeProvider({
  children,
  defaultTheme = "system",
  storageKey = "agent-session-manager-theme",
}: ThemeProviderProps) {
  const resolveSystemAppearance = (): "light" | "dark" =>
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";

  const getInitialTheme = (): "light" | "dark" => {
    const fallback =
      defaultTheme === "light" || defaultTheme === "dark"
        ? defaultTheme
        : resolveSystemAppearance();
    if (typeof window === "undefined") {
      return fallback;
    }

    const stored = safeLocalStorageGet(storageKey) as Theme | null;
    if (stored === "light" || stored === "dark") {
      return stored;
    }

    return fallback;
  };

  const [theme, setThemeState] = useState<"light" | "dark">(getInitialTheme);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    safeLocalStorageSet(storageKey, theme);
  }, [theme, storageKey]);

  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    const root = window.document.documentElement;
    root.classList.remove("light", "dark");
    root.classList.add(theme);
  }, [theme]);

  // Sync native window theme (Windows/macOS title bar)
  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }

    let isCancelled = false;

    const updateNativeTheme = async (nativeTheme: string) => {
      if (isCancelled) return;
      try {
        await invoke("set_window_theme", { theme: nativeTheme });
      } catch (e) {
        // Ignore errors (e.g., when not running in Tauri)
        console.debug("Failed to set native window theme:", e);
      }
    };

    void updateNativeTheme(theme);

    return () => {
      isCancelled = true;
    };
  }, [theme]);

  const value = useMemo<ThemeContextValue>(
    () => ({
      theme,
      resolvedTheme: theme,
      setTheme: (nextTheme: Theme) => {
        const concrete =
          nextTheme === "system" ? resolveSystemAppearance() : nextTheme;
        if (concrete === theme) return;
        setThemeState(concrete);
      },
    }),
    [theme],
  );

  return (
    <ThemeProviderContext.Provider value={value}>
      {children}
    </ThemeProviderContext.Provider>
  );
}

export function useTheme() {
  const context = useContext(ThemeProviderContext);
  if (context === undefined) {
    throw new Error("useTheme must be used within a ThemeProvider");
  }
  return context;
}
