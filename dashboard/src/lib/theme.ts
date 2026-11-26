// Light and dark themes. The system setting is followed unless the person
// picks one; the choice is remembered on this device.

export type ThemeChoice = 'system' | 'light' | 'dark';

const KEY = 'bella.theme';
const ORDER: ThemeChoice[] = ['system', 'light', 'dark'];

export function readTheme(): ThemeChoice {
  try {
    const stored = window.localStorage.getItem(KEY);
    if (stored === 'light' || stored === 'dark') return stored;
  } catch {
    // Storage blocked: follow the system.
  }
  return 'system';
}

export function applyTheme(choice: ThemeChoice): void {
  const root = document.documentElement;
  if (choice === 'system') delete root.dataset.theme;
  else root.dataset.theme = choice;
  try {
    if (choice === 'system') window.localStorage.removeItem(KEY);
    else window.localStorage.setItem(KEY, choice);
  } catch {
    // Applies to this page only.
  }
}

export function nextTheme(choice: ThemeChoice): ThemeChoice {
  return ORDER[(ORDER.indexOf(choice) + 1) % ORDER.length] ?? 'system';
}
