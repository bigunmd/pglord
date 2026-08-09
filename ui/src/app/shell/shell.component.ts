import { Component, inject, signal } from '@angular/core';
import { RouterLink, RouterLinkActive, RouterOutlet } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideCheck, lucideLogOut, lucideMonitor, lucideMoon, lucideSun } from '@ng-icons/lucide';
import { HlmButtonImports } from '@spartan-ng/helm/button';
import { HlmDropdownMenuImports } from '@spartan-ng/helm/dropdown-menu';

import { WhoamiResponse } from '../api/models/whoami-response';
import { AuthService } from '../core/auth/auth.service';
import { OidcService } from '../core/auth/oidc.service';
import { ThemeMode, ThemeService } from '../core/theme/theme.service';
import { PglordLogoComponent } from './pglord-logo.component';

@Component({
  selector: 'pg-shell',
  imports: [
    RouterLink,
    RouterLinkActive,
    RouterOutlet,
    TranslocoModule,
    NgIcon,
    HlmButtonImports,
    HlmDropdownMenuImports,
    PglordLogoComponent,
  ],
  providers: [provideIcons({ lucideCheck, lucideLogOut, lucideMonitor, lucideMoon, lucideSun })],
  template: `
    <div class="flex min-h-svh flex-col">
      <header class="border-border bg-card flex h-14 items-center gap-3 border-b px-4 lg:px-6">
        <a routerLink="/" class="flex items-center gap-2">
          <pg-logo [size]="26" />
          <span class="text-primary font-semibold tracking-tight">
            {{ 'app.title' | transloco }}
          </span>
        </a>
        <span class="text-muted-foreground hidden text-sm sm:inline">
          {{ 'app.tagline' | transloco }}
        </span>
        <nav class="ml-6 flex items-center gap-1 text-sm">
          <a
            routerLink="/"
            routerLinkActive="text-foreground font-medium"
            [routerLinkActiveOptions]="{ exact: true }"
            class="text-muted-foreground hover:text-foreground rounded-md px-3 py-1.5 transition-colors"
          >
            {{ 'nav.home' | transloco }}
          </a>
          <a
            routerLink="/clusters"
            routerLinkActive="text-foreground font-medium"
            class="text-muted-foreground hover:text-foreground rounded-md px-3 py-1.5 transition-colors"
          >
            {{ 'nav.clusters' | transloco }}
          </a>
          <a
            routerLink="/preflight"
            routerLinkActive="text-foreground font-medium"
            class="text-muted-foreground hover:text-foreground rounded-md px-3 py-1.5 transition-colors"
          >
            {{ 'nav.permissions' | transloco }}
          </a>
        </nav>
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
          @if (auth.current()) {
            @if (who(); as me) {
              <span
                class="text-muted-foreground mr-1 hidden text-xs sm:inline"
                [title]="'auth.signedInAs' | transloco: { user: me.user || me.mode }"
                data-testid="signed-in-as"
              >
                {{ me.display_name || me.user }}
              </span>
            }
            <button
              hlmBtn
              variant="ghost"
              size="icon"
              [attr.aria-label]="'auth.logout' | transloco"
              (click)="logout()"
            >
              <ng-icon name="lucideLogOut" size="18" />
            </button>
          }
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
  protected readonly auth = inject(AuthService);
  private readonly oidc = inject(OidcService);

  /** Who the server says we are. Blank until it answers; a failure here must
   * not break the header, so it degrades to showing nothing. */
  protected readonly who = signal<WhoamiResponse | null>(null);

  constructor() {
    if (this.auth.current()) {
      this.auth
        .whoami()
        .then((me) => this.who.set(me.mode === 'none' ? null : me))
        .catch(() => this.who.set(null));
    }
  }

  protected logout(): void {
    // Goes through OidcService so an IdP session is ended too when the
    // issuer supports it; it falls back to a local clear otherwise.
    this.oidc.logout();
  }

  protected readonly themeModes: { mode: ThemeMode; icon: string; labelKey: string }[] = [
    { mode: 'system', icon: 'lucideMonitor', labelKey: 'theme.system' },
    { mode: 'light', icon: 'lucideSun', labelKey: 'theme.light' },
    { mode: 'dark', icon: 'lucideMoon', labelKey: 'theme.dark' },
  ];
}
