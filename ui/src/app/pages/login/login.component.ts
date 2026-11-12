import { Component, inject, signal } from '@angular/core';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButtonImports } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { Api } from '../../api/api';
import { version } from '../../api/fn/system/version';
import { AuthConfigResponse } from '../../api/models/auth-config-response';
import { AuthService } from '../../core/auth/auth.service';
import { OidcService } from '../../core/auth/oidc.service';
import { PglordLogoComponent } from '../../shell/pglord-logo.component';

/** Login screen: token paste and/or OIDC redirect, driven by what the server
 * says it accepts. Lives outside the shell — no nav, no theme menu. */
@Component({
  selector: 'pg-login',
  imports: [TranslocoModule, HlmButtonImports, HlmInput, HlmLabel, PglordLogoComponent],
  template: `
    <main class="grid min-h-svh lg:grid-cols-[1fr_1fr]">
      <section class="flex flex-col justify-between bg-[#142635] p-8 text-white sm:p-12 lg:p-16">
        <div class="flex items-center gap-3">
          <pg-logo [size]="40" />
          <span class="text-2xl font-semibold tracking-tight">{{ 'app.title' | transloco }}</span>
        </div>
        <div class="max-w-lg py-10 lg:py-24">
          <p class="mb-6 text-xs font-medium tracking-widest text-[#b8d7ed] uppercase">
            {{ 'app.tagline' | transloco }}
          </p>
          <h1 class="text-4xl leading-tight font-semibold tracking-[-0.05em] sm:text-5xl">
            {{ 'ux.login.heading' | transloco }}
          </h1>
          <p class="mt-6 max-w-sm text-base leading-relaxed text-[#c1cfdb]">
            {{ 'ux.login.intro' | transloco }}
          </p>
        </div>
        <p class="hidden text-xs text-[#c1cfdb] lg:block">{{ 'ux.shell.console' | transloco }}</p>
      </section>
      <section class="flex items-center justify-center p-6 sm:p-12">
        <div class="w-full max-w-sm py-8">
          <h2 class="pg-page-title">{{ 'ux.login.welcome' | transloco }}</h2>
          <p class="pg-page-description mb-8">{{ 'auth.title' | transloco }}</p>

          @if (config(); as cfg) {
            @if (cfg.static_tokens_enabled || cfg.kubernetes_enabled) {
              <form (submit)="submitToken($event)">
                <label hlmLabel class="mb-1.5 block" for="api-token">
                  {{ 'auth.tokenLabel' | transloco }}
                </label>
                <input
                  hlmInput
                  id="api-token"
                  type="password"
                  autocomplete="off"
                  class="mb-2 h-11 w-full"
                  aria-describedby="token-help"
                  [value]="tokenInput()"
                  (input)="tokenInput.set($any($event.target).value)"
                />
                <p id="token-help" class="text-muted-foreground mb-4 text-xs leading-relaxed">
                  {{ 'auth.tokenHelp' | transloco }}
                </p>
                @if (cfg.kubernetes_enabled) {
                  <p class="text-muted-foreground mb-3 text-xs" data-testid="k8s-hint">
                    {{ 'auth.kubernetesHint' | transloco }}
                  </p>
                }
                <button hlmBtn type="submit" class="w-full" [disabled]="busy() || !tokenInput()">
                  {{ 'auth.signIn' | transloco }}
                </button>
              </form>
            }
            @if ((cfg.static_tokens_enabled || cfg.kubernetes_enabled) && cfg.oidc) {
              <div class="text-muted-foreground my-4 flex items-center gap-3 text-xs">
                <span class="border-border grow border-t"></span>
                {{ 'auth.or' | transloco }}
                <span class="border-border grow border-t"></span>
              </div>
            }
            @if (cfg.oidc; as oidc) {
              <button
                hlmBtn
                variant="outline"
                class="w-full"
                [disabled]="busy()"
                (click)="oidcLogin(oidc.issuer, oidc.client_id, cfg.scopes)"
              >
                {{ 'auth.signInWith' | transloco: { host: issuerHost(oidc.issuer) } }}
              </button>
            }
          } @else if (!error()) {
            <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
          }

          @if (error(); as err) {
            <p class="text-destructive mt-3 text-sm" role="alert">{{ err | transloco }}</p>
          }
          <p class="text-muted-foreground mt-8 border-t pt-5 text-xs leading-relaxed">
            {{ 'ux.login.secure' | transloco }}
          </p>
        </div>
      </section>
    </main>
  `,
})
export class LoginComponent {
  private readonly api = inject(Api);
  private readonly auth = inject(AuthService);
  private readonly oidc = inject(OidcService);
  private readonly router = inject(Router);

  protected readonly config = signal<AuthConfigResponse | null>(null);
  protected readonly tokenInput = signal('');
  protected readonly error = signal<string | null>(null);
  protected readonly busy = signal(false);

  constructor() {
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const cfg = await this.auth.authConfig();
      if (!(await this.auth.enabled())) {
        // Auth is off server-side: there is nothing to log in to. Note this
        // has to agree with the guard, or the two bounce the user forever.
        void this.router.navigate(['/']);
        return;
      }
      this.config.set(cfg);
    } catch {
      this.error.set('auth.configUnreachable');
    }
  }

  protected async submitToken(event: Event): Promise<void> {
    event.preventDefault();
    this.busy.set(true);
    this.error.set(null);
    this.auth.setStatic(this.tokenInput().trim());
    try {
      // Any authenticated endpoint works as a probe; version is the cheapest.
      await this.api.invoke(version);
      void this.router.navigate(['/']);
    } catch {
      this.auth.logout();
      this.error.set('auth.badToken');
    } finally {
      this.busy.set(false);
    }
  }

  protected async oidcLogin(issuer: string, clientId: string, scopes: string): Promise<void> {
    this.busy.set(true);
    this.error.set(null);
    try {
      await this.oidc.beginLogin(issuer, clientId, scopes);
    } catch {
      this.error.set('auth.oidcUnreachable');
      this.busy.set(false);
    }
  }

  protected issuerHost(issuer: string): string {
    try {
      return new URL(issuer).host;
    } catch {
      return issuer;
    }
  }
}
