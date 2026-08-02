import { Component, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { TlsSpec } from '../../api/models/tls-spec';

export interface TlsDialogContext {
  tls: TlsSpec;
}

/** Point the cluster at a kubernetes.io/tls Secret (or clear it to disable
 *  TLS). Resolves to the full replacement block, or undefined on cancel. */
@Component({
  selector: 'pg-cluster-tls-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    HlmButton,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
    HlmInput,
    HlmLabel,
  ],
  template: `
    <div hlmDialogHeader class="border-border border-b pb-4">
      <h2 hlmDialogTitle>{{ 'tls.editTitle' | transloco }}</h2>
    </div>

    <div class="grid gap-4 py-1">
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'tls.explainer' | transloco }}
      </p>
      <div class="grid gap-2">
        <label hlmLabel for="tls-secret">{{ 'tls.serverSecret' | transloco }}</label>
        <input
          hlmInput
          id="tls-secret"
          data-testid="tls-server-secret"
          [placeholder]="'tls.serverSecretPlaceholder' | transloco"
          [ngModel]="serverSecret()"
          (ngModelChange)="serverSecret.set($event)"
        />
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'tls.serverSecretHelp' | transloco }}
        </p>
      </div>
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'tls.rollWarning' | transloco }}
      </p>
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="tls-submit" (click)="submit()">
        {{ 'tls.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterTlsDialogComponent {
  protected readonly ref = inject<BrnDialogRef<TlsSpec | undefined>>(BrnDialogRef);
  protected readonly ctx = injectBrnDialogContext<TlsDialogContext>();

  protected readonly serverSecret = signal(this.ctx.tls?.server_secret ?? '');

  protected submit(): void {
    const name = this.serverSecret().trim();
    // Full replacement: an empty block disables TLS.
    this.ref.close(name ? { server_secret: name } : {});
  }
}
