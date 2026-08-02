import { Component, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { RoleSpec } from '../../api/models/role-spec';
import { DatabaseSpec } from '../../api/models/database-spec';

export interface ProvisionDialogContext {
  roles: RoleSpec[];
  databases: DatabaseSpec[];
}

export interface ProvisionResult {
  roles: RoleSpec[];
  databases: DatabaseSpec[];
}

interface RoleRow {
  name: string;
  login: boolean;
  createdb: boolean;
  createrole: boolean;
  connectionLimit: string; // empty = unlimited
  secretName: string;
  secretKey: string;
  memberships: string; // comma-separated
}

interface DbRow {
  name: string;
  owner: string;
}

/** Edit the declared roles and databases. Resolves to the full replacement
 *  lists, or undefined on cancel. Additive semantics: removing a row stops
 *  managing the object, nothing is dropped. */
@Component({
  selector: 'pg-cluster-provision-dialog',
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
      <h2 hlmDialogTitle>{{ 'provision.editTitle' | transloco }}</h2>
    </div>

    <div class="grid max-h-[60vh] gap-5 overflow-y-auto py-1 pr-1">
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'provision.explainer' | transloco }}
      </p>

      <div class="grid gap-3">
        <h3 class="text-sm font-medium">{{ 'provision.roles' | transloco }}</h3>
        @for (row of roleRows(); track $index; let i = $index) {
          <div class="border-border grid gap-2 rounded-md border p-3">
            <div class="grid grid-cols-2 gap-2">
              <div class="grid gap-1">
                <label hlmLabel [for]="'role-name-' + i">{{
                  'provision.roleName' | transloco
                }}</label>
                <input
                  hlmInput
                  [id]="'role-name-' + i"
                  [attr.data-testid]="'role-name-' + i"
                  placeholder="app_rw"
                  [ngModel]="row.name"
                  (ngModelChange)="patchRole(i, { name: $event })"
                />
              </div>
              <div class="grid gap-1">
                <label hlmLabel [for]="'role-limit-' + i">{{
                  'provision.connectionLimit' | transloco
                }}</label>
                <input
                  hlmInput
                  [id]="'role-limit-' + i"
                  type="number"
                  min="-1"
                  [placeholder]="'provision.unlimited' | transloco"
                  [ngModel]="row.connectionLimit"
                  (ngModelChange)="patchRole(i, { connectionLimit: $event })"
                />
              </div>
            </div>
            <div class="flex flex-wrap gap-4">
              <label hlmLabel class="flex items-center gap-2">
                <input
                  type="checkbox"
                  class="accent-primary"
                  [ngModel]="row.login"
                  (ngModelChange)="patchRole(i, { login: $event })"
                />
                {{ 'provision.login' | transloco }}
              </label>
              <label hlmLabel class="flex items-center gap-2">
                <input
                  type="checkbox"
                  class="accent-primary"
                  [ngModel]="row.createdb"
                  (ngModelChange)="patchRole(i, { createdb: $event })"
                />
                {{ 'provision.createdb' | transloco }}
              </label>
              <label hlmLabel class="flex items-center gap-2">
                <input
                  type="checkbox"
                  class="accent-primary"
                  [ngModel]="row.createrole"
                  (ngModelChange)="patchRole(i, { createrole: $event })"
                />
                {{ 'provision.createrole' | transloco }}
              </label>
            </div>
            <div class="grid grid-cols-2 gap-2">
              <div class="grid gap-1">
                <label hlmLabel [for]="'role-secret-' + i">{{
                  'provision.secretName' | transloco
                }}</label>
                <input
                  hlmInput
                  [id]="'role-secret-' + i"
                  placeholder="app-creds"
                  [ngModel]="row.secretName"
                  (ngModelChange)="patchRole(i, { secretName: $event })"
                />
              </div>
              <div class="grid gap-1">
                <label hlmLabel [for]="'role-key-' + i">{{
                  'provision.secretKey' | transloco
                }}</label>
                <input
                  hlmInput
                  [id]="'role-key-' + i"
                  placeholder="password"
                  [ngModel]="row.secretKey"
                  (ngModelChange)="patchRole(i, { secretKey: $event })"
                />
              </div>
            </div>
            <div class="grid gap-1">
              <label hlmLabel [for]="'role-member-' + i">{{
                'provision.memberships' | transloco
              }}</label>
              <input
                hlmInput
                [id]="'role-member-' + i"
                placeholder="reporting, analytics"
                [ngModel]="row.memberships"
                (ngModelChange)="patchRole(i, { memberships: $event })"
              />
            </div>
            <button
              hlmBtn
              variant="ghost"
              size="sm"
              class="justify-self-end"
              (click)="removeRole(i)"
            >
              {{ 'provision.remove' | transloco }}
            </button>
          </div>
        }
        <button hlmBtn variant="outline" size="sm" data-testid="add-role" (click)="addRole()">
          {{ 'provision.addRole' | transloco }}
        </button>
      </div>

      <div class="grid gap-3">
        <h3 class="text-sm font-medium">{{ 'provision.databases' | transloco }}</h3>
        @for (row of dbRows(); track $index; let i = $index) {
          <div class="grid grid-cols-[1fr_1fr_auto] items-end gap-2">
            <div class="grid gap-1">
              <label hlmLabel [for]="'db-name-' + i">{{ 'provision.dbName' | transloco }}</label>
              <input
                hlmInput
                [id]="'db-name-' + i"
                [attr.data-testid]="'db-name-' + i"
                placeholder="appdb"
                [ngModel]="row.name"
                (ngModelChange)="patchDb(i, { name: $event })"
              />
            </div>
            <div class="grid gap-1">
              <label hlmLabel [for]="'db-owner-' + i">{{ 'provision.owner' | transloco }}</label>
              <select
                [id]="'db-owner-' + i"
                class="border-input bg-background h-9 rounded-md border px-3 text-sm"
                [ngModel]="row.owner"
                (ngModelChange)="patchDb(i, { owner: $event })"
              >
                <option value="">—</option>
                @for (name of roleNames(); track name) {
                  <option [value]="name">{{ name }}</option>
                }
              </select>
            </div>
            <button hlmBtn variant="ghost" size="sm" (click)="removeDb(i)">
              {{ 'provision.remove' | transloco }}
            </button>
          </div>
        }
        <button hlmBtn variant="outline" size="sm" data-testid="add-database" (click)="addDb()">
          {{ 'provision.addDatabase' | transloco }}
        </button>
      </div>
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="provision-submit" (click)="submit()">
        {{ 'provision.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterProvisionDialogComponent {
  protected readonly ref = inject<BrnDialogRef<ProvisionResult | undefined>>(BrnDialogRef);
  protected readonly ctx = injectBrnDialogContext<ProvisionDialogContext>();

  protected readonly roleRows = signal<RoleRow[]>(
    (this.ctx.roles ?? []).map((r) => ({
      name: r.name,
      login: r.login ?? true,
      createdb: r.createdb ?? false,
      createrole: r.createrole ?? false,
      connectionLimit: r.connection_limit != null ? String(r.connection_limit) : '',
      secretName: r.password_secret?.name ?? '',
      secretKey: r.password_secret?.key ?? '',
      memberships: (r.in_roles ?? []).join(', '),
    })),
  );
  protected readonly dbRows = signal<DbRow[]>(
    (this.ctx.databases ?? []).map((d) => ({ name: d.name, owner: d.owner })),
  );

  protected roleNames(): string[] {
    return this.roleRows()
      .map((r) => r.name.trim())
      .filter(Boolean);
  }

  protected patchRole(i: number, patch: Partial<RoleRow>): void {
    this.roleRows.update((rows) => rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  }
  protected patchDb(i: number, patch: Partial<DbRow>): void {
    this.dbRows.update((rows) => rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  }
  protected addRole(): void {
    this.roleRows.update((rows) => [
      ...rows,
      {
        name: '',
        login: true,
        createdb: false,
        createrole: false,
        connectionLimit: '',
        secretName: '',
        secretKey: '',
        memberships: '',
      },
    ]);
  }
  protected addDb(): void {
    this.dbRows.update((rows) => [...rows, { name: '', owner: '' }]);
  }
  protected removeRole(i: number): void {
    this.roleRows.update((rows) => rows.filter((_, j) => j !== i));
  }
  protected removeDb(i: number): void {
    this.dbRows.update((rows) => rows.filter((_, j) => j !== i));
  }

  protected submit(): void {
    const roles: RoleSpec[] = this.roleRows()
      .filter((r) => r.name.trim())
      .map((r) => ({
        name: r.name.trim(),
        login: r.login,
        createdb: r.createdb,
        createrole: r.createrole,
        ...(r.connectionLimit.trim() !== ''
          ? { connection_limit: Number(r.connectionLimit) }
          : {}),
        ...(r.secretName.trim() && r.secretKey.trim()
          ? { password_secret: { name: r.secretName.trim(), key: r.secretKey.trim() } }
          : {}),
        in_roles: r.memberships
          .split(',')
          .map((m) => m.trim())
          .filter(Boolean),
      }));
    const databases: DatabaseSpec[] = this.dbRows()
      .filter((d) => d.name.trim() && d.owner.trim())
      .map((d) => ({ name: d.name.trim(), owner: d.owner.trim() }));
    this.ref.close({ roles, databases });
  }
}
