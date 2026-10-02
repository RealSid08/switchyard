import { useCallback, useSyncExternalStore } from 'react';

export type ThemePref = 'system' | 'light' | 'dark';
const KEY = 'switchyard.theme';
const listeners = new Set<() => void>();

function read(): ThemePref {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'light' || v === 'dark' ? v : 'system';
  } catch {
    return 'system';
  }
}

function systemDark() {
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? true;
}

/** Applies the resolved theme to <html>. index.html runs the same logic before first paint. */
export function applyTheme(pref = read()) {
  const resolved = pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

if (typeof window !== 'undefined') {
  window.matchMedia?.('(prefers-color-scheme: dark)').addEventListener('change', () => {
    applyTheme();
    listeners.forEach((l) => l());
  });
}

export function useTheme() {
  const pref = useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    read,
    () => 'system' as ThemePref,
  );
  const setPref = useCallback((next: ThemePref) => {
    try {
      if (next === 'system') localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, next);
    } catch {
      // ignore
    }
    applyTheme(next);
    listeners.forEach((l) => l());
  }, []);
  const resolved: 'light' | 'dark' = pref === 'system' ? (systemDark() ? 'dark' : 'light') : pref;
  return { pref, resolved, setPref };
}
