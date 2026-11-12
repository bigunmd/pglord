import { Component, inject, signal } from '@angular/core';
import { NavigationEnd, Router, RouterLink, RouterOutlet } from '@angular/router';
import { toSignal } from '@angular/core/rxjs-interop';
import { filter, map } from 'rxjs';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import {
  lucideCheck,
  lucideDatabase,
  lucideLayoutDashboard,
  lucideLogOut,
  lucideMonitor,
  lucideMoon,
  lucideShieldCheck,
  lucideSun,
} from '@ng-icons/lucide';
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
    RouterOutlet,
    TranslocoModule,
    NgIcon,
    HlmButtonImports,
    HlmDropdownMenuImports,
    PglordLogoComponent,
  ],
  providers: [
    provideIcons({
      lucideCheck,
      lucideDatabase,
      lucideLayoutDashboard,
      lucideLogOut,
      lucideMonitor,
      lucideMoon,
      lucideShieldCheck,
      lucideSun,
    }),
  ],
  template: `
    <a
      class="pg-skip-link"
      href="#main-content"
      (click)="$event.preventDefault(); main.focus(); main.scrollIntoView()"
      >{{ 'ux.shell.skip' | transloco }}</a
    >
    <div class="pg-shell">
      <aside class="pg-sidebar">
        <a routerLink="/" class="pg-brand" [attr.aria-label]="'app.title' | transloco">
          <pg-logo [size]="36" />
          <span class="pg-brand-name">{{ 'app.title' | transloco }}</span>
        </a>
        <div>
          <p class="pg-nav-label hidden lg:block">{{ 'ux.shell.workspace' | transloco }}</p>
          <nav [attr.aria-label]="'ux.shell.navigation' | transloco">
            @for (item of navigation; track item.path) {
              <a
                [routerLink]="item.path"
                [class.is-active]="active(item.path)"
                [attr.aria-current]="active(item.path) ? 'page' : null"
                class="pg-nav-link"
              >
                <ng-icon [name]="item.icon" size="19" aria-hidden="true" />
                {{ item.label | transloco }}
              </a>
            }
          </nav>
        </div>
        <div class="mt-auto hidden border-t border-white/10 px-2 pt-5 lg:block">
          <p class="text-xs font-medium text-white">{{ 'ux.shell.console' | transloco }}</p>
          <p class="mt-2 text-xs leading-relaxed">{{ 'app.tagline' | transloco }}</p>
        </div>
      </aside>
      <div class="pg-shell-body">
        <header class="pg-topbar">
          <p class="text-muted-foreground text-xs font-medium sm:text-sm">
            {{ 'ux.shell.console' | transloco }}
          </p>
          <div class="flex min-w-0 items-center gap-2">
            @if (auth.current()) {
              @if (who(); as me) {
                <span
                  class="text-muted-foreground hidden max-w-48 truncate text-xs sm:inline"
                  [title]="'auth.signedInAs' | transloco: { user: me.user || me.mode }"
                  data-testid="signed-in-as"
                  >{{ me.display_name || me.user }}</span
                >
              }
            }
            <button
              hlmBtn
              variant="ghost"
              size="icon"
              [hlmDropdownMenuTrigger]="themeMenu"
              [attr.aria-label]="'theme.label' | transloco"
            >
              <ng-icon
                [name]="theme.resolved() === 'dark' ? 'lucideMoon' : 'lucideSun'"
                size="18"
                aria-hidden="true"
              />
            </button>
            <ng-template #themeMenu>
              <hlm-dropdown-menu class="w-44">
                <div hlmDropdownMenuLabel>{{ 'theme.label' | transloco }}</div>
                @for (t of themeModes; track t.mode) {
                  <button hlmDropdownMenuItem (click)="theme.setMode(t.mode)">
                    <ng-icon [name]="t.icon" size="16" aria-hidden="true" />
                    {{ t.labelKey | transloco }}
                    @if (theme.mode() === t.mode) {
                      <ng-icon name="lucideCheck" size="16" class="ml-auto" aria-hidden="true" />
                      <span class="sr-only">{{ 'ux.shell.selected' | transloco }}</span>
                    }
                  </button>
                }
              </hlm-dropdown-menu>
            </ng-template>
            @if (auth.current()) {
              <button
                hlmBtn
                variant="ghost"
                size="icon"
                [attr.aria-label]="'auth.logout' | transloco"
                (click)="logout()"
              >
                <ng-icon name="lucideLogOut" size="18" aria-hidden="true" />
              </button>
            }
          </div>
        </header>
        <main #main id="main-content" tabindex="-1" class="pg-main">
          <router-outlet />
        </main>
      </div>
    </div>
  `,
})
export class ShellComponent {
  private readonly router = inject(Router);
  private readonly currentUrl = toSignal(
    this.router.events.pipe(
      filter((event): event is NavigationEnd => event instanceof NavigationEnd),
      map((event) => event.urlAfterRedirects),
    ),
    { initialValue: this.router.url },
  );

  protected active(path: string): boolean {
    const current = this.currentUrl().split(/[?#]/)[0];
    return path === '/'
      ? current === '/'
      : current.startsWith(path) || (path === '/clusters' && current.startsWith('/namespaces/'));
  }

  protected readonly theme = inject(ThemeService);
  protected readonly auth = inject(AuthService);
  private readonly oidc = inject(OidcService);
  protected readonly who = signal<WhoamiResponse | null>(null);
  protected readonly navigation = [
    { path: '/', label: 'ux.shell.overview', icon: 'lucideLayoutDashboard' },
    { path: '/clusters', label: 'nav.clusters', icon: 'lucideDatabase' },
    { path: '/preflight', label: 'nav.permissions', icon: 'lucideShieldCheck' },
  ];

  constructor() {
    if (this.auth.current()) {
      this.auth
        .whoami()
        .then((me) => this.who.set(me.mode === 'none' ? null : me))
        .catch(() => this.who.set(null));
    }
  }

  protected logout(): void {
    this.oidc.logout();
  }

  protected readonly themeModes: { mode: ThemeMode; icon: string; labelKey: string }[] = [
    { mode: 'system', icon: 'lucideMonitor', labelKey: 'theme.system' },
    { mode: 'light', icon: 'lucideSun', labelKey: 'theme.light' },
    { mode: 'dark', icon: 'lucideMoon', labelKey: 'theme.dark' },
  ];
}
