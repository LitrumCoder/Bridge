import { en } from './i18n/en';
import { type Dict, ru } from './i18n/ru';

export type Lang = 'ru' | 'en';
export type Theme = 'system' | 'light' | 'dark';

const DICTS: Record<Lang, Dict> = { ru, en };

function stored<T extends string>(key: string, allowed: readonly T[], fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    if (v && (allowed as readonly string[]).includes(v)) return v as T;
  } catch {
    // Storage can be unavailable (private mode, tests); defaults then.
  }
  return fallback;
}

function persist(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Best effort only.
  }
}

function defaultLang(): Lang {
  return typeof navigator !== 'undefined' && navigator.language?.startsWith('ru') ? 'ru' : 'en';
}

class Settings {
  lang = $state<Lang>(stored('bridge.lang', ['ru', 'en'], defaultLang()));
  theme = $state<Theme>(stored('bridge.theme', ['system', 'light', 'dark'], 'system'));
  t = $derived(DICTS[this.lang]);

  setLang(lang: Lang) {
    this.lang = lang;
    persist('bridge.lang', lang);
    document.documentElement.lang = lang;
  }

  setTheme(theme: Theme) {
    this.theme = theme;
    persist('bridge.theme', theme);
    applyTheme(theme);
  }
}

export function applyTheme(theme: Theme) {
  const root = document.documentElement;
  if (theme === 'system') {
    root.removeAttribute('data-theme');
  } else {
    root.setAttribute('data-theme', theme);
  }
}

export const settings = new Settings();
