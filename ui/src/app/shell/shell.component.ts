import { Component, inject } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideCheck, lucideMonitor, lucideMoon, lucideSun } from '@ng-icons/lucide';
import { HlmButtonImports } from '@spartan-ng/helm/button';
import { HlmDropdownMenuImports } from '@spartan-ng/helm/dropdown-menu';

import { ThemeMode, ThemeService } from '../core/theme/theme.service';

@Component({
  selector: 'pg-shell',
  imports: [RouterOutlet, TranslocoModule, NgIcon, HlmButtonImports, HlmDropdownMenuImports],
  providers: [provideIcons({ lucideCheck, lucideMonitor, lucideMoon, lucideSun })],
  template: `
    <div class="flex min-h-svh flex-col">
      <header class="border-border bg-card flex h-14 items-center gap-3 border-b px-4 lg:px-6">
        <span class="text-primary font-semibold tracking-tight">
          {{ 'app.title' | transloco }}
        </span>
        <span class="text-muted-foreground hidden text-sm sm:inline">
          {{ 'app.tagline' | transloco }}
        </span>
        <div class="ml-auto">
          <button hlmBtn variant="ghost" size="icon" [hlmDropdownMenuTrigger]="themeMenu">
            <ng-icon [name]="theme.resolved() === 'dark' ? 'lucideMoon' : 'lucideSun'" size="18" />
          </button>
          <ng-template #themeMenu>
            <hlm-dropdown-menu class="w-40">
              <div hlmDropdownMenuLabel>{{ 'theme.label' | transloco }}</div>
              @for (t of themeModes; track t.mode) {
                <button hlmDropdownMenuItem (click)="theme.setMode(t.mode)">
                  <ng-icon [name]="t.icon" size="16" />
                  {{ t.labelKey | transloco }}
                  @if (theme.mode() === t.mode) {
                    <ng-icon name="lucideCheck" size="16" class="ml-auto" />
                  }
                </button>
              }
            </hlm-dropdown-menu>
          </ng-template>
        </div>
      </header>
      <main class="flex-1 p-4 lg:p-6">
        <router-outlet />
      </main>
    </div>
  `,
})
export class ShellComponent {
  protected readonly theme = inject(ThemeService);

  protected readonly themeModes: { mode: ThemeMode; icon: string; labelKey: string }[] = [
    { mode: 'system', icon: 'lucideMonitor', labelKey: 'theme.system' },
    { mode: 'light', icon: 'lucideSun', labelKey: 'theme.light' },
    { mode: 'dark', icon: 'lucideMoon', labelKey: 'theme.dark' },
  ];
}
