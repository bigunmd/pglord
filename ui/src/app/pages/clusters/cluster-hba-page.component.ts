import { Component, computed, inject, input, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';

import { Api } from '../../api/api';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { patchCluster } from '../../api/fn/clusters/patch-cluster';
import { HbaRule } from '../../api/models/hba-rule';
import { problemDetail } from '../../core/problem';
import { BreadcrumbsComponent } from '../../shell/breadcrumbs.component';

const CONNS: HbaRule['conn'][] = ['host', 'hostssl', 'hostnossl'];
const METHODS: HbaRule['method'][] = ['scram-sha-256', 'reject', 'trust', 'cert'];

/** Edit a cluster's pg_hba rules. Order matters (first match wins), so rows
 * keep the order the user gives them — no sorting. */
@Component({
  selector: 'pg-cluster-hba-page',
  imports: [FormsModule, TranslocoModule, NgIcon, HlmButton, HlmInput, BreadcrumbsComponent],
  providers: [provideIcons({ lucidePlus, lucideTrash2 })],
  template: `
    <div class="mx-auto max-w-3xl pb-24">
      <pg-breadcrumbs [crumbs]="crumbs()" />
      <h1 class="mb-1 text-lg font-semibold">{{ 'hba.editTitle' | transloco }}</h1>
      <p class="text-muted-foreground mb-6 text-sm">{{ 'hba.intro' | transloco }}</p>

      @if (error(); as err) {
        <p class="text-destructive mb-4 text-sm" role="alert">{{ err }}</p>
      }

      @if (loading()) {
        <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
      } @else {
        <div class="grid gap-4">
          <p class="text-muted-foreground text-xs leading-relaxed">
            {{ 'hba.explainer' | transloco }}
          </p>

          @for (row of rows(); track $index; let i = $index) {
            <div
              class="grid items-end gap-2 sm:grid-cols-[7rem_1fr_1fr_1fr_9rem_auto]"
              [attr.data-testid]="'hba-row-' + i"
            >
              <select
                class="border-input bg-background h-9 rounded-md border px-2 text-sm"
                [attr.aria-label]="'hba.conn' | transloco"
                [ngModel]="row.conn"
                (ngModelChange)="set(i, 'conn', $event)"
              >
                @for (c of conns; track c) {
                  <option [value]="c">{{ c }}</option>
                }
              </select>
              <input
                hlmInput
                class="font-mono text-xs"
                [attr.aria-label]="'hba.database' | transloco"
                [placeholder]="'hba.databasePlaceholder' | transloco"
                [ngModel]="row.database"
                (ngModelChange)="set(i, 'database', $event)"
              />
              <input
                hlmInput
                class="font-mono text-xs"
                [attr.aria-label]="'hba.user' | transloco"
                [placeholder]="'hba.userPlaceholder' | transloco"
                [ngModel]="row.user"
                (ngModelChange)="set(i, 'user', $event)"
              />
              <input
                hlmInput
                class="font-mono text-xs"
                [attr.aria-label]="'hba.address' | transloco"
                [placeholder]="'hba.addressPlaceholder' | transloco"
                [ngModel]="row.address"
                (ngModelChange)="set(i, 'address', $event)"
              />
              <select
                class="border-input bg-background h-9 rounded-md border px-2 text-sm"
                [attr.aria-label]="'hba.method' | transloco"
                [ngModel]="row.method"
                (ngModelChange)="set(i, 'method', $event)"
              >
                @for (m of methods; track m) {
                  <option [value]="m">{{ m }}</option>
                }
              </select>
              <button
                hlmBtn
                variant="ghost"
                size="icon-sm"
                [attr.aria-label]="'hba.remove' | transloco"
                [attr.data-testid]="'hba-remove-' + i"
                (click)="remove(i)"
              >
                <ng-icon name="lucideTrash2" size="14" />
              </button>
            </div>
          } @empty {
            <p class="text-muted-foreground py-2 text-sm">{{ 'hba.noneSet' | transloco }}</p>
          }

          <div class="border-border border-t pt-4">
            <button hlmBtn variant="outline" data-testid="hba-add" (click)="add()">
              <ng-icon name="lucidePlus" size="14" />
              {{ 'hba.add' | transloco }}
            </button>
          </div>
        </div>
      }
    </div>

    <!-- Sticky action bar: on a long form the primary action must stay
         reachable without scrolling back down. -->
    <div class="bg-background/95 border-border fixed inset-x-0 bottom-0 border-t backdrop-blur">
      <div class="mx-auto flex max-w-3xl items-center gap-3 px-4 py-3">
        <div class="ml-auto flex gap-2">
          <button hlmBtn variant="ghost" (click)="cancel()">
            {{ 'common.cancel' | transloco }}
          </button>
          <button
            hlmBtn
            data-testid="hba-submit"
            [disabled]="loading() || busy() || !valid()"
            (click)="submit()"
          >
            {{ 'hba.apply' | transloco }}
          </button>
        </div>
      </div>
    </div>
  `,
})
export class ClusterHbaPageComponent {
  private readonly api = inject(Api);
  private readonly router = inject(Router);

  public readonly ns = input.required<string>();
  public readonly name = input.required<string>();

  protected readonly loading = signal(true);
  protected readonly busy = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly crumbs = computed(() => [
    { label: 'Clusters', link: '/clusters' },
    { label: this.name(), link: '/namespaces/' + this.ns() + '/clusters/' + this.name() },
    { label: 'Access rules' },
  ]);

  protected readonly conns = CONNS;
  protected readonly methods = METHODS;
  protected readonly rows = signal<HbaRule[]>([]);

  protected readonly valid = computed(() =>
    this.rows().every(
      (r) =>
        r.database.trim().length > 0 && r.user.trim().length > 0 && r.address.trim().length > 0,
    ),
  );

  constructor() {
    queueMicrotask(() => void this.load());
  }

  private async load(): Promise<void> {
    try {
      const d = await this.api.invoke(getCluster, { ns: this.ns(), name: this.name() });
      this.rows.set((d.pg_hba ?? []).map((r) => ({ ...r })));
      this.error.set(null);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.loading.set(false);
    }
  }

  protected add(): void {
    this.rows.update((rows) => [
      ...rows,
      { conn: 'host', database: 'all', user: 'all', address: '', method: 'scram-sha-256' },
    ]);
  }

  protected remove(i: number): void {
    this.rows.update((rows) => rows.filter((_, idx) => idx !== i));
  }

  protected set(i: number, field: keyof HbaRule, value: string): void {
    this.rows.update((rows) =>
      rows.map((r, idx) => (idx === i ? ({ ...r, [field]: value } as HbaRule) : r)),
    );
  }

  protected cancel(): void {
    void this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
  }

  protected async submit(): Promise<void> {
    const rules = this.rows().map((r) => ({
      ...r,
      database: r.database.trim(),
      user: r.user.trim(),
      address: r.address.trim(),
    }));

    this.busy.set(true);
    this.error.set(null);
    try {
      await this.api.invoke(patchCluster, {
        ns: this.ns(),
        name: this.name(),
        body: { pg_hba: rules },
      });
      await this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.busy.set(false);
    }
  }
}
