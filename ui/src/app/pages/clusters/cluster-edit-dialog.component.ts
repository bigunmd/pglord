import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { SpecChange } from '../../api/models/spec-change';

const STORAGE_RE = /^[0-9]+(Ki|Mi|Gi|Ti)$/;
const REPLICA_CHOICES = [1, 2, 3, 4, 5] as const;

export interface ClusterEditContext {
  replicas: number;
  storageSize: string;
}

/** Edit a running cluster. Resolves to the change, or undefined on cancel. */
@Component({
  selector: 'pg-cluster-edit-dialog',
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
      <h2 hlmDialogTitle>{{ 'clusterDetail.editTitle' | transloco }}</h2>
    </div>
    <div class="grid gap-3 py-2">
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
              [attr.data-testid]="'edit-replicas-' + n"
              (click)="replicas.set(n)"
            >
              {{ n }}
            </button>
          }
        </div>
        <p class="text-xs" [class.text-destructive]="shrinking()" [class.text-muted-foreground]="!shrinking()">
          {{ (shrinking() ? 'clusterDetail.scaleDownWarning' : 'clusters.replicasHint') | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-edit-storage">{{ 'clusters.storage' | transloco }}</label>
        <input
          hlmInput
          id="pg-edit-storage"
          data-testid="edit-storage"
          [ngModel]="storage()"
          (ngModelChange)="storage.set($event)"
        />
        <p
          class="text-xs"
          [class.text-destructive]="!storageValid()"
          [class.text-muted-foreground]="storageValid()"
        >
          {{ 'clusterDetail.storageGrowOnly' | transloco }}
        </p>
      </div>
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="edit-submit" [disabled]="!canSave()" (click)="submit()">
        {{ 'clusterDetail.applyChanges' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterEditDialogComponent {
  protected readonly ref = inject<BrnDialogRef<SpecChange | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<ClusterEditContext>();

  protected readonly replicaChoices = REPLICA_CHOICES;
  protected readonly replicas = signal(this.ctx.replicas);
  protected readonly storage = signal(this.ctx.storageSize);

  protected readonly storageValid = computed(() => STORAGE_RE.test(this.storage()));
  protected readonly shrinking = computed(() => this.replicas() < this.ctx.replicas);
  protected readonly canSave = computed(
    () =>
      this.storageValid() &&
      (this.replicas() !== this.ctx.replicas || this.storage() !== this.ctx.storageSize),
  );

  submit(): void {
    const change: SpecChange = {};
    if (this.replicas() !== this.ctx.replicas) {
      change.replicas = this.replicas();
    }
    if (this.storage() !== this.ctx.storageSize) {
      change.storage_size = this.storage();
    }
    this.ref.close(change);
  }
}
