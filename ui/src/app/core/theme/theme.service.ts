import { Injectable, computed, effect, signal } from '@angular/core';

export type ThemeMode = 'system' | 'light' | 'dark';

interface PersistedSettings {
  theme?: ThemeMode;
}

const STORAGE_KEY = 'pglord.settings';

/**
 * Theme state: `mode` is the user's choice, `resolved` is what is actually on
 * screen (system resolves against the OS preference, live). Persisted to
 * localStorage; the inline script in index.html reads the same key before
 * first paint to avoid a flash of the wrong theme.
 */
@Injectable({ providedIn: 'root' })
export class ThemeService {
  private readonly systemDark = signal(window.matchMedia('(prefers-color-scheme: dark)').matches);

  readonly mode = signal<ThemeMode>(this.readPersisted());
  readonly resolved = computed<'light' | 'dark'>(() => {
    if (this.mode() === 'system') {
      return this.systemDark() ? 'dark' : 'light';
    }
    return this.mode() === 'dark' ? 'dark' : 'light';
  });

  constructor() {
    window
      .matchMedia('(prefers-color-scheme: dark)')
      .addEventListener('change', (e) => this.systemDark.set(e.matches));

    effect(() => {
      const mode = this.mode();
      document.documentElement.classList.toggle('dark', this.resolved() === 'dark');
      document.documentElement.style.colorScheme = mode === 'system' ? 'light dark' : mode;
    });
  }

  setMode(mode: ThemeMode): void {
    this.mode.set(mode);
    const settings: PersistedSettings = { theme: mode };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  }

  private readPersisted(): ThemeMode {
    try {
      const settings = JSON.parse(
        localStorage.getItem(STORAGE_KEY) ?? '{}',
      ) as PersistedSettings;
      return settings.theme ?? 'system';
    } catch {
      return 'system';
    }
  }
}
