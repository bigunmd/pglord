import { Component, inject, signal } from '@angular/core';
import { ActivatedRoute, Router, RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButtonImports } from '@spartan-ng/helm/button';

import { OidcService } from '../../core/auth/oidc.service';

/** OIDC redirect target: exchanges the one-time code for tokens, then heads
 * to the app. Only ever visited mid-login. */
@Component({
  selector: 'pg-login-callback',
  imports: [TranslocoModule, RouterLink, HlmButtonImports],
  template: `
    <div class="flex min-h-svh items-center justify-center p-4">
      <div class="text-center">
        @if (error(); as err) {
          <p class="text-destructive mb-3 text-sm" role="alert">{{ err }}</p>
          <a hlmBtn variant="outline" routerLink="/login">{{ 'auth.backToLogin' | transloco }}</a>
        } @else {
          <p class="text-muted-foreground text-sm">{{ 'auth.completing' | transloco }}</p>
        }
      </div>
    </div>
  `,
})
export class LoginCallbackComponent {
  private readonly oidc = inject(OidcService);
  private readonly router = inject(Router);
  private readonly route = inject(ActivatedRoute);

  protected readonly error = signal<string | null>(null);

  constructor() {
    void this.complete();
  }

  private async complete(): Promise<void> {
    const params = this.route.snapshot.queryParamMap;
    const idpError = params.get('error');
    if (idpError) {
      this.error.set(params.get('error_description') ?? idpError);
      return;
    }
    const code = params.get('code');
    const state = params.get('state');
    if (!code || !state) {
      this.error.set('Missing code or state in the callback URL.');
      return;
    }
    try {
      await this.oidc.completeLogin(code, state);
      void this.router.navigate(['/']);
    } catch (err) {
      this.error.set(err instanceof Error ? err.message : 'Login failed.');
    }
  }
}
