import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { EnvVarSpec } from '../../api/models/env-var-spec';
import { InitSpec } from '../../api/models/init-spec';
import {
  EnvEditorComponent,
  EnvRow,
  rowsToSpec,
  rowsValid,
  specToRows,
} from './env-editor.component';

export interface EnvDialogContext {
  credentialsSecret?: string | null;
  env: EnvVarSpec[];
  init: InitSpec;
  /** Editing a cluster that already exists: credentials are fixed by then. */
  existing: boolean;
  generatedName: string;
}

export interface EnvDialogResult {
  credentials_secret?: string;
  env: EnvVarSpec[];
  init: InitSpec;
}

/** Configure credentials, environment and init scripts. Resolves to the blocks. */
@Component({
  selector: 'pg-cluster-env-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    HlmButton,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
    HlmInput,
    HlmLabel,
    EnvEditorComponent,
  ],
  template: `
    <div hlmDialogHeader class="border-border border-b pb-4">
      <h2 hlmDialogTitle>{{ 'secrets.editTitle' | transloco }}</h2>
    </div>

    <div class="pg-scroll grid max-h-[min(65vh,32rem)] gap-6 overflow-y-auto py-1 pr-2">
      <div class="grid gap-2">
        <label hlmLabel for="ev-creds">{{ 'secrets.credentialsSecret' | transloco }}</label>
        <input
          hlmInput
          id="ev-creds"
          data-testid="credentials-secret"
          [placeholder]="ctx.generatedName"
          [disabled]="ctx.existing"
          [ngModel]="credentialsSecret()"
          (ngModelChange)="credentialsSecret.set($event)"
        />
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{
            (ctx.existing ? 'secrets.credentialsSecretFixed' : 'secrets.credentialsSecretHelp')
              | transloco
          }}
        </p>
      </div>

      <div class="border-border border-t pt-6">
        <pg-env-editor [(rows)]="rows" />
      </div>

      <div class="border-border grid gap-4 border-t pt-6">
        <span hlmLabel>{{ 'secrets.init' | transloco }}</span>
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'secrets.initHelp' | transloco }}
        </p>
        <div class="grid gap-2">
          <label hlmLabel for="ev-init-cm">{{ 'secrets.initConfigMap' | transloco }}</label>
          <input
            hlmInput
            id="ev-init-cm"
            data-testid="init-configmap"
            [ngModel]="initConfigMap()"
            (ngModelChange)="initConfigMap.set($event)"
          />
        </div>
        <div class="grid gap-2">
          <label hlmLabel for="ev-init-secret">{{ 'secrets.initSecret' | transloco }}</label>
          <input
            hlmInput
            id="ev-init-secret"
            data-testid="init-secret"
            [ngModel]="initSecret()"
            (ngModelChange)="initSecret.set($event)"
          />
          <p class="text-muted-foreground text-xs leading-relaxed">
            {{ 'secrets.initSecretHelp' | transloco }}
          </p>
        </div>
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'secrets.initOnceNote' | transloco }}
        </p>
      </div>
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="env-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'secrets.save' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterEnvDialogComponent {
  protected readonly ref = inject<BrnDialogRef<EnvDialogResult | undefined>>(BrnDialogRef);
  protected readonly ctx = injectBrnDialogContext<EnvDialogContext>();

  protected readonly credentialsSecret = signal(this.ctx.credentialsSecret ?? '');
  protected readonly initConfigMap = signal(this.ctx.init?.scripts_config_map ?? '');
  protected readonly initSecret = signal(this.ctx.init?.scripts_secret ?? '');
  protected readonly rows = signal<EnvRow[]>(specToRows(this.ctx.env));

  protected readonly valid = computed(() => rowsValid(this.rows()));

  protected submit(): void {
    const trimmed = (s: string) => (s.trim() ? s.trim() : undefined);
    this.ref.close({
      credentials_secret: trimmed(this.credentialsSecret()),
      env: rowsToSpec(this.rows()),
      init: {
        scripts_config_map: trimmed(this.initConfigMap()),
        scripts_secret: trimmed(this.initSecret()),
      },
    });
  }
}
