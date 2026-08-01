import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';

import { HbaRule } from '../../api/models/hba-rule';

export interface HbaDialogContext {
  rules: HbaRule[];
}

const CONNS: HbaRule['conn'][] = ['host', 'hostssl', 'hostnossl'];
const METHODS: HbaRule['method'][] = ['scram-sha-256', 'reject', 'trust'];

/** Edit a cluster's pg_hba rules. Resolves to the full rule list, or
 * undefined on cancel. Order matters (first match wins), so rows keep the
 * order the user gives them — no sorting. */
@Component({
  selector: 'pg-cluster-hba-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    NgIcon,
    HlmButton,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
    HlmInput,
  ],
  providers: [provideIcons({ lucidePlus, lucideTrash2 })],
  template: `
    <div hlmDialogHeader class="border-border border-b pb-4">
      <h2 hlmDialogTitle>{{ 'hba.editTitle' | transloco }}</h2>
    </div>

    <div class="pg-scroll grid max-h-[min(65vh,32rem)] gap-4 overflow-y-auto py-1 pr-2">
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

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="hba-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'hba.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterHbaDialogComponent {
  protected readonly ref = inject<BrnDialogRef<HbaRule[] | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<HbaDialogContext>();

  protected readonly conns = CONNS;
  protected readonly methods = METHODS;
  protected readonly rows = signal<HbaRule[]>(this.ctx.rules.map((r) => ({ ...r })));

  protected readonly valid = computed(() =>
    this.rows().every(
      (r) =>
        r.database.trim().length > 0 && r.user.trim().length > 0 && r.address.trim().length > 0,
    ),
  );

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

  protected submit(): void {
    this.ref.close(
      this.rows().map((r) => ({
        ...r,
        database: r.database.trim(),
        user: r.user.trim(),
        address: r.address.trim(),
      })),
    );
  }
}
