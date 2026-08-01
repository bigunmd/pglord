import { Component, computed, model } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { EnvVarSpec } from '../../api/models/env-var-spec';

/** One editable row. `fromSecret` picks which of the two sources is in use. */
export interface EnvRow {
  name: string;
  fromSecret: boolean;
  value: string;
  secretName: string;
  secretKey: string;
}

/**
 * Names pglord sets on every instance itself, mirroring
 * crates/pglord-core/src/env.rs. Checked here as well as in the API so the
 * reason appears next to the field instead of as a failed request.
 */
const RESERVED_PREFIXES = ['PGLORD_', 'PGBACKREST_'];
const RESERVED_NAMES = [
  'PGDATA',
  'PGHOST',
  'PGHOSTADDR',
  'PGPORT',
  'PGUSER',
  'PGPASSWORD',
  'PGPASSFILE',
  'PGDATABASE',
  'PGOPTIONS',
  'PGSERVICE',
  'PGSERVICEFILE',
  'PGSSLMODE',
  'PGSSLROOTCERT',
  'PGREQUIRESSL',
  'PGCONNECT_TIMEOUT',
  'POSTGRES_USER',
  'POSTGRES_PASSWORD',
  'POSTGRES_DB',
  'POSTGRES_INITDB_ARGS',
  'PATH',
  'LD_PRELOAD',
  'LD_LIBRARY_PATH',
];

export function isReservedEnvName(name: string): boolean {
  const upper = name.trim().toUpperCase();
  return RESERVED_PREFIXES.some((p) => upper.startsWith(p)) || RESERVED_NAMES.includes(upper);
}

export function specToRows(env: EnvVarSpec[] | undefined): EnvRow[] {
  return (env ?? []).map((e) => ({
    name: e.name,
    fromSecret: !!e.secret,
    value: e.value ?? '',
    secretName: e.secret?.name ?? '',
    secretKey: e.secret?.key ?? '',
  }));
}

export function rowsToSpec(rows: EnvRow[]): EnvVarSpec[] {
  return rows.map((r) =>
    r.fromSecret
      ? { name: r.name.trim(), secret: { name: r.secretName.trim(), key: r.secretKey.trim() } }
      : { name: r.name.trim(), value: r.value },
  );
}

/** Translation key for what is wrong with a row, or null. */
export function rowError(row: EnvRow, all: EnvRow[]): string | null {
  const name = row.name.trim();
  if (!name) {
    return null; // an untouched new row is not an error yet
  }
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
    return 'secrets.errors.name';
  }
  if (isReservedEnvName(name)) {
    return 'secrets.errors.reserved';
  }
  if (all.filter((r) => r.name.trim() === name).length > 1) {
    return 'secrets.errors.duplicate';
  }
  if (row.fromSecret && (!row.secretName.trim() || !row.secretKey.trim())) {
    return 'secrets.errors.incompleteSecret';
  }
  return null;
}

export function rowsValid(rows: EnvRow[]): boolean {
  return rows.every((r) => r.name.trim() !== '' && rowError(r, rows) === null);
}

/** Editor for the custom environment. Shared by the create and edit dialogs. */
@Component({
  selector: 'pg-env-editor',
  imports: [FormsModule, TranslocoModule, HlmButton, HlmInput, HlmLabel],
  template: `
    <div class="grid gap-4">
      <div class="flex items-center justify-between">
        <span hlmLabel>{{ 'secrets.env' | transloco }}</span>
        <button hlmBtn variant="outline" size="sm" data-testid="env-add" (click)="addRow()">
          {{ 'workload.addRow' | transloco }}
        </button>
      </div>
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'secrets.envHelp' | transloco }}
      </p>

      @for (row of rows(); track $index) {
        <div class="border-border grid gap-2 rounded-lg border p-3">
          <div class="flex items-center gap-2">
            <input
              hlmInput
              class="font-mono"
              [attr.data-testid]="'env-name-' + $index"
              [placeholder]="'secrets.envName' | transloco"
              [ngModel]="row.name"
              (ngModelChange)="update($index, { name: $event })"
            />
            <button
              hlmBtn
              variant="ghost"
              size="sm"
              [attr.data-testid]="'env-remove-' + $index"
              (click)="removeRow($index)"
            >
              {{ 'workload.removeRow' | transloco }}
            </button>
          </div>

          <div class="bg-muted grid grid-cols-2 gap-1 rounded-lg p-1" role="tablist">
            <button
              type="button"
              class="cursor-pointer rounded-md px-2 py-1.5 text-sm font-medium transition-colors"
              [class.bg-background]="!row.fromSecret"
              [class.shadow-sm]="!row.fromSecret"
              [class.text-muted-foreground]="row.fromSecret"
              role="tab"
              [attr.aria-selected]="!row.fromSecret"
              [attr.data-testid]="'env-source-value-' + $index"
              (click)="update($index, { fromSecret: false })"
            >
              {{ 'secrets.envValue' | transloco }}
            </button>
            <button
              type="button"
              class="cursor-pointer rounded-md px-2 py-1.5 text-sm font-medium transition-colors"
              [class.bg-background]="row.fromSecret"
              [class.shadow-sm]="row.fromSecret"
              [class.text-muted-foreground]="!row.fromSecret"
              role="tab"
              [attr.aria-selected]="row.fromSecret"
              [attr.data-testid]="'env-source-secret-' + $index"
              (click)="update($index, { fromSecret: true })"
            >
              {{ 'secrets.envFromSecret' | transloco }}
            </button>
          </div>

          @if (row.fromSecret) {
            <div class="grid gap-2 sm:grid-cols-2">
              <input
                hlmInput
                [attr.data-testid]="'env-secret-' + $index"
                [placeholder]="'secrets.envSecretName' | transloco"
                [ngModel]="row.secretName"
                (ngModelChange)="update($index, { secretName: $event })"
              />
              <input
                hlmInput
                [attr.data-testid]="'env-key-' + $index"
                [placeholder]="'secrets.envSecretKey' | transloco"
                [ngModel]="row.secretKey"
                (ngModelChange)="update($index, { secretKey: $event })"
              />
            </div>
          } @else {
            <input
              hlmInput
              [attr.data-testid]="'env-value-' + $index"
              [placeholder]="'secrets.envValue' | transloco"
              [ngModel]="row.value"
              (ngModelChange)="update($index, { value: $event })"
            />
          }

          @if (error(row); as err) {
            <p class="text-destructive text-xs leading-relaxed" role="alert">
              {{ err | transloco: { name: row.name } }}
            </p>
          }
        </div>
      }
    </div>
  `,
})
export class EnvEditorComponent {
  readonly rows = model<EnvRow[]>([]);

  protected readonly valid = computed(() => rowsValid(this.rows()));

  protected error(row: EnvRow): string | null {
    return rowError(row, this.rows());
  }

  protected addRow(): void {
    this.rows.update((rows) => [
      ...rows,
      { name: '', fromSecret: false, value: '', secretName: '', secretKey: '' },
    ]);
  }

  protected removeRow(index: number): void {
    this.rows.update((rows) => rows.filter((_, i) => i !== index));
  }

  protected update(index: number, patch: Partial<EnvRow>): void {
    this.rows.update((rows) => rows.map((r, i) => (i === index ? { ...r, ...patch } : r)));
  }
}
