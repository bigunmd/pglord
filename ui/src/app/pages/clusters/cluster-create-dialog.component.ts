import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { ClusterSpec } from '../../api/models/cluster-spec';

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
  ],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusters.createTitle' | transloco }}</h2>
    </div>
    <div class="grid gap-3 py-2">
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
        @if (name() && !nameValid()) {
          <p class="text-destructive text-xs">{{ 'clusters.nameInvalid' | transloco }}</p>
        }
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
        @if (storage() && !storageValid()) {
          <p class="text-destructive text-xs">{{ 'clusters.storageInvalid' | transloco }}</p>
        }
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-image">{{ 'clusters.image' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-image"
          [ngModel]="image()"
          (ngModelChange)="image.set($event)"
          placeholder="pglord-agent:dev"
        />
      </div>
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
  protected readonly image = signal('pglord-agent:dev');

  protected readonly nameValid = computed(() => NAME_RE.test(this.name()));
  protected readonly storageValid = computed(() => STORAGE_RE.test(this.storage()));
  protected readonly valid = computed(
    () => this.nameValid() && this.storageValid() && this.image().trim().length > 0,
  );

  submit(): void {
    this.ref.close({
      name: this.name(),
      replicas: this.replicas(),
      storage_size: this.storage(),
      image: this.image().trim(),
    });
  }
}
