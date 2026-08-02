import { Component, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { PoolerSpec } from '../../api/models/pooler-spec';

export interface PoolerDialogContext {
  pooler: PoolerSpec;
}

/** Edit the managed PgBouncer block. Resolves to the full replacement block,
 *  or undefined on cancel. Disabling sends an explicit enabled:false block so
 *  the reconciler deletes the pooler objects. */
@Component({
  selector: 'pg-cluster-pooler-dialog',
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
      <h2 hlmDialogTitle>{{ 'pooler.editTitle' | transloco }}</h2>
    </div>

    <div class="grid gap-4 py-1">
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'pooler.explainer' | transloco }}
      </p>
      <label hlmLabel class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-primary"
          data-testid="pooler-enabled"
          [ngModel]="enabled()"
          (ngModelChange)="enabled.set($event)"
        />
        {{ 'pooler.enable' | transloco }}
      </label>

      @if (enabled()) {
        <div class="grid grid-cols-2 gap-4">
          <div class="grid gap-2">
            <label hlmLabel for="pooler-replicas">{{ 'pooler.replicas' | transloco }}</label>
            <input
              hlmInput
              id="pooler-replicas"
              data-testid="pooler-replicas"
              type="number"
              min="1"
              max="10"
              [ngModel]="replicas()"
              (ngModelChange)="replicas.set($event)"
            />
          </div>
          <div class="grid gap-2">
            <label hlmLabel for="pooler-mode">{{ 'pooler.poolMode' | transloco }}</label>
            <select
              id="pooler-mode"
              data-testid="pooler-mode"
              class="border-input bg-background h-9 rounded-md border px-3 text-sm"
              [ngModel]="poolMode()"
              (ngModelChange)="poolMode.set($event)"
            >
              <option value="transaction">{{ 'pooler.modeTransaction' | transloco }}</option>
              <option value="session">{{ 'pooler.modeSession' | transloco }}</option>
            </select>
          </div>
          <div class="grid gap-2">
            <label hlmLabel for="pooler-max-conn">{{ 'pooler.maxClientConn' | transloco }}</label>
            <input
              hlmInput
              id="pooler-max-conn"
              data-testid="pooler-max-conn"
              type="number"
              min="1"
              [ngModel]="maxClientConn()"
              (ngModelChange)="maxClientConn.set($event)"
            />
          </div>
          <div class="grid gap-2">
            <label hlmLabel for="pooler-pool-size">{{ 'pooler.defaultPoolSize' | transloco }}</label>
            <input
              hlmInput
              id="pooler-pool-size"
              data-testid="pooler-pool-size"
              type="number"
              min="1"
              [ngModel]="defaultPoolSize()"
              (ngModelChange)="defaultPoolSize.set($event)"
            />
          </div>
        </div>
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'pooler.modeHelp' | transloco }}
        </p>
      }
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="pooler-submit" (click)="submit()">
        {{ 'pooler.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterPoolerDialogComponent {
  protected readonly ref = inject<BrnDialogRef<PoolerSpec | undefined>>(BrnDialogRef);
  protected readonly ctx = injectBrnDialogContext<PoolerDialogContext>();

  protected readonly enabled = signal(this.ctx.pooler.enabled ?? false);
  protected readonly replicas = signal(this.ctx.pooler.replicas ?? 1);
  protected readonly poolMode = signal(this.ctx.pooler.pool_mode ?? 'transaction');
  protected readonly maxClientConn = signal(this.ctx.pooler.max_client_conn ?? 100);
  protected readonly defaultPoolSize = signal(this.ctx.pooler.default_pool_size ?? 20);

  protected submit(): void {
    if (!this.enabled()) {
      // Full replacement: an explicit disabled block removes the pooler.
      this.ref.close({ enabled: false });
      return;
    }
    this.ref.close({
      enabled: true,
      replicas: Number(this.replicas()) || 1,
      pool_mode: this.poolMode(),
      max_client_conn: Number(this.maxClientConn()) || 100,
      default_pool_size: Number(this.defaultPoolSize()) || 20,
    });
  }
}
