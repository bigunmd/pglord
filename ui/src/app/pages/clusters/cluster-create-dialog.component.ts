import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { ClusterSpec } from '../../api/models/cluster-spec';
import { EnvEditorComponent, EnvRow, rowsToSpec, rowsValid } from './env-editor.component';

/** Mirrors the server's RFC1123 rule (crates/pglord-core/src/spec.rs). */
const NAME_RE = /^[a-z0-9]([-a-z0-9]{0,38}[a-z0-9])?$/;
const STORAGE_RE = /^[0-9]+(Ki|Mi|Gi|Ti)$/;
const REPLICA_CHOICES = [1, 2, 3, 4, 5] as const;

/** Collects a ClusterSpec; resolves to it on create, undefined on cancel. */
@Component({
  selector: 'pg-cluster-create-dialog',
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
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusters.createTitle' | transloco }}</h2>
    </div>
    <div class="grid gap-6 py-1">
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-name">{{ 'clusters.name' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-name"
          data-testid="cluster-name"
          [ngModel]="name()"
          (ngModelChange)="name.set($event)"
          placeholder="orders-db"
          autocomplete="off"
        />
        <!-- Persistent helper text: the same string is always rendered and only
             recolored on error, so nothing reflows regardless of wrapping. -->
        <p
          class="text-xs"
          [class.text-muted-foreground]="!name() || nameValid()"
          [class.text-destructive]="name() && !nameValid()"
        >
          {{ 'clusters.nameHelp' | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <span hlmLabel>{{ 'clusters.replicas' | transloco }}</span>
        <div class="bg-muted grid grid-cols-5 gap-1 rounded-lg p-1" role="tablist">
          @for (n of replicaChoices; track n) {
            <button
              type="button"
              class="cursor-pointer rounded-md px-3 py-1.5 font-mono text-sm font-medium transition-colors"
              [class.bg-background]="replicas() === n"
              [class.shadow-sm]="replicas() === n"
              [class.text-muted-foreground]="replicas() !== n"
              role="tab"
              [attr.aria-selected]="replicas() === n"
              (click)="replicas.set(n)"
            >
              {{ n }}
            </button>
          }
        </div>
        <p class="text-muted-foreground text-xs">{{ 'clusters.replicasHint' | transloco }}</p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-storage">{{ 'clusters.storage' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-storage"
          [ngModel]="storage()"
          (ngModelChange)="storage.set($event)"
          placeholder="1Gi"
        />
        <p
          class="text-xs"
          [class.text-muted-foreground]="!storage() || storageValid()"
          [class.text-destructive]="storage() && !storageValid()"
        >
          {{ 'clusters.storageHelp' | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-image">{{ 'clusters.image' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-image"
          [ngModel]="image()"
          (ngModelChange)="image.set($event)"
          [placeholder]="'clusters.imageDefault' | transloco"
        />
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'clusters.imageHelp' | transloco }}
        </p>
      </div>

      <!-- Collapsed by default: most clusters need none of this, but init
           scripts and their environment can only be set here, since they run
           when the data directory is first created. -->
      <div class="border-border border-t pt-4">
        <button
          hlmBtn
          variant="ghost"
          size="sm"
          class="-ml-2"
          data-testid="toggle-advanced"
          [attr.aria-expanded]="advanced()"
          (click)="advanced.set(!advanced())"
        >
          {{ (advanced() ? 'clusters.hideAdvanced' : 'clusters.showAdvanced') | transloco }}
        </button>
      </div>

      @if (advanced()) {
        <div class="grid gap-6">
          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-creds">
              {{ 'secrets.credentialsSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-creds"
              data-testid="create-credentials-secret"
              [placeholder]="(name() || 'orders-db') + '-credentials'"
              [ngModel]="credentialsSecret()"
              (ngModelChange)="credentialsSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.credentialsSecretHelp' | transloco }}
            </p>
          </div>

          <pg-env-editor [(rows)]="rows" />

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-init-cm">
              {{ 'secrets.initConfigMap' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-init-cm"
              data-testid="create-init-configmap"
              [ngModel]="initConfigMap()"
              (ngModelChange)="initConfigMap.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.initHelp' | transloco }}
            </p>
          </div>

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-init-secret">
              {{ 'secrets.initSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-init-secret"
              data-testid="create-init-secret"
              [ngModel]="initSecret()"
              (ngModelChange)="initSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.initSecretHelp' | transloco }}
            </p>
          </div>

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-tls-secret">
              {{ 'tls.serverSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-tls-secret"
              data-testid="create-tls-secret"
              [placeholder]="'tls.serverSecretPlaceholder' | transloco"
              [ngModel]="tlsSecret()"
              (ngModelChange)="tlsSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'tls.serverSecretHelp' | transloco }}
            </p>
          </div>

          <label hlmLabel class="flex items-center gap-2">
            <input
              type="checkbox"
              class="accent-primary"
              data-testid="create-pooler-enabled"
              [ngModel]="poolerEnabled()"
              (ngModelChange)="poolerEnabled.set($event)"
            />
            {{ 'pooler.enable' | transloco }}
          </label>
          <p class="text-muted-foreground -mt-2 text-xs leading-relaxed">
            {{ 'pooler.explainer' | transloco }}
          </p>
        </div>
      }
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="cluster-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'clusters.create' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterCreateDialogComponent {
  protected readonly ref = inject<BrnDialogRef<ClusterSpec | undefined>>(BrnDialogRef);

  protected readonly replicaChoices = REPLICA_CHOICES;
  protected readonly name = signal('');
  protected readonly replicas = signal<number>(2);
  protected readonly storage = signal('1Gi');
  // Empty means "not specified": the request omits the field and the server
  // supplies the image this deployment was configured with.
  protected readonly image = signal('');

  protected readonly advanced = signal(false);
  protected readonly credentialsSecret = signal('');
  protected readonly initConfigMap = signal('');
  protected readonly initSecret = signal('');
  protected readonly tlsSecret = signal('');
  protected readonly poolerEnabled = signal(false);
  protected readonly rows = signal<EnvRow[]>([]);

  protected readonly nameValid = computed(() => NAME_RE.test(this.name()));
  protected readonly storageValid = computed(() => STORAGE_RE.test(this.storage()));
  protected readonly valid = computed(
    () =>
      this.nameValid() && this.storageValid() && rowsValid(this.rows()),
  );

  submit(): void {
    const trimmed = (s: string) => (s.trim() ? s.trim() : undefined);
    this.ref.close({
      name: this.name(),
      replicas: this.replicas(),
      storage_size: this.storage(),
      ...(this.image().trim() ? { image: this.image().trim() } : {}),
      credentials_secret: trimmed(this.credentialsSecret()),
      env: rowsToSpec(this.rows()),
      init: {
        scripts_config_map: trimmed(this.initConfigMap()),
        scripts_secret: trimmed(this.initSecret()),
      },
      tls: { server_secret: trimmed(this.tlsSecret()) },
      ...(this.poolerEnabled() ? { pooler: { enabled: true } } : {}),
    });
  }
}
