import { Component, computed, inject, input, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { Api } from '../../api/api';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { patchCluster } from '../../api/fn/clusters/patch-cluster';
import { RoleSpec } from '../../api/models/role-spec';
import { DatabaseSpec } from '../../api/models/database-spec';
import { ExtensionSpec } from '../../api/models/extension-spec';
import { problemDetail } from '../../core/problem';
import { BreadcrumbsComponent } from '../../shell/breadcrumbs.component';

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

interface ExtRow {
  name: string;
  database: string;
  schema: string;
  preload: boolean;
}

/** Edit the declared roles, databases and extensions. Three growable lists
 *  need room and a shareable URL, so this is a page rather than a dialog.
 *  Additive semantics: removing a row stops managing the object, nothing is
 *  ever dropped. */
@Component({
  selector: 'pg-cluster-provision-page',
  imports: [FormsModule, TranslocoModule, HlmButton, HlmInput, HlmLabel, BreadcrumbsComponent],
  template: `
    <div class="mx-auto max-w-3xl pb-24">
      <pg-breadcrumbs [crumbs]="crumbs()" />
      <h1 class="mb-1 text-lg font-semibold">{{ 'provision.editTitle' | transloco }}</h1>
      <p class="text-muted-foreground mb-6 text-sm">{{ 'provision.intro' | transloco }}</p>

      @if (error(); as err) {
        <p class="text-destructive mb-4 text-sm" role="alert">{{ err }}</p>
      }

      @if (loading()) {
        <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
      } @else {
        <div class="grid gap-5">
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
                  <label hlmLabel [for]="'db-name-' + i">{{
                    'provision.dbName' | transloco
                  }}</label>
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
                  <label hlmLabel [for]="'db-owner-' + i">{{
                    'provision.owner' | transloco
                  }}</label>
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

          <div class="grid gap-3">
            <h3 class="text-sm font-medium">{{ 'provision.extensions' | transloco }}</h3>
            <p class="text-muted-foreground text-xs">{{ 'provision.preloadHint' | transloco }}</p>
            @for (row of extRows(); track $index; let i = $index) {
              <div class="grid grid-cols-[1fr_1fr_1fr_auto_auto] items-end gap-2">
                <div class="grid gap-1">
                  <label hlmLabel [for]="'ext-name-' + i">{{
                    'provision.extensionName' | transloco
                  }}</label>
                  <input
                    hlmInput
                    [id]="'ext-name-' + i"
                    [attr.data-testid]="'ext-name-' + i"
                    placeholder="pgcrypto"
                    [ngModel]="row.name"
                    (ngModelChange)="patchExt(i, { name: $event })"
                  />
                </div>
                <div class="grid gap-1">
                  <label hlmLabel [for]="'ext-db-' + i">{{
                    'provision.extensionDatabase' | transloco
                  }}</label>
                  <select
                    [id]="'ext-db-' + i"
                    class="border-input bg-background h-9 rounded-md border px-3 text-sm"
                    [ngModel]="row.database"
                    (ngModelChange)="patchExt(i, { database: $event })"
                  >
                    <option value="postgres">postgres</option>
                    @for (name of dbNames(); track name) {
                      <option [value]="name">{{ name }}</option>
                    }
                  </select>
                </div>
                <div class="grid gap-1">
                  <label hlmLabel [for]="'ext-schema-' + i">{{
                    'provision.extensionSchema' | transloco
                  }}</label>
                  <input
                    hlmInput
                    [id]="'ext-schema-' + i"
                    placeholder="public"
                    [ngModel]="row.schema"
                    (ngModelChange)="patchExt(i, { schema: $event })"
                  />
                </div>
                <label hlmLabel class="flex items-center gap-2 pb-2">
                  <input
                    type="checkbox"
                    class="accent-primary"
                    [attr.data-testid]="'ext-preload-' + i"
                    [ngModel]="row.preload"
                    (ngModelChange)="patchExt(i, { preload: $event })"
                  />
                  {{ 'provision.extensionPreload' | transloco }}
                </label>
                <button hlmBtn variant="ghost" size="sm" (click)="removeExt(i)">
                  {{ 'provision.remove' | transloco }}
                </button>
              </div>
            }
            <button
              hlmBtn
              variant="outline"
              size="sm"
              data-testid="add-extension"
              (click)="addExt()"
            >
              {{ 'provision.addExtension' | transloco }}
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
            data-testid="provision-submit"
            [disabled]="loading() || busy()"
            (click)="submit()"
          >
            {{ 'provision.apply' | transloco }}
          </button>
        </div>
      </div>
    </div>
  `,
})
export class ClusterProvisionPageComponent {
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
    { label: 'Provisioning' },
  ]);

  protected readonly roleRows = signal<RoleRow[]>([]);
  protected readonly dbRows = signal<DbRow[]>([]);
  protected readonly extRows = signal<ExtRow[]>([]);

  constructor() {
    queueMicrotask(() => void this.load());
  }

  /** Seeds the form from the live spec: the page can be reached by URL, so it
   * cannot rely on a caller handing it the current lists. */
  private async load(): Promise<void> {
    try {
      const d = await this.api.invoke(getCluster, { ns: this.ns(), name: this.name() });
      this.roleRows.set(
        (d.roles ?? []).map((r) => ({
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
      this.dbRows.set((d.databases ?? []).map((db) => ({ name: db.name, owner: db.owner })));
      this.extRows.set(
        (d.extensions ?? []).map((e) => ({
          name: e.name,
          database: e.database,
          schema: e.schema ?? '',
          preload: e.preload ?? false,
        })),
      );
      this.error.set(null);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.loading.set(false);
    }
  }

  protected dbNames(): string[] {
    return this.dbRows()
      .map((d) => d.name.trim())
      .filter(Boolean);
  }

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
  protected patchExt(i: number, patch: Partial<ExtRow>): void {
    this.extRows.update((rows) => rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  }
  protected addExt(): void {
    this.extRows.update((rows) => [
      ...rows,
      { name: '', database: 'postgres', schema: '', preload: false },
    ]);
  }
  protected removeExt(i: number): void {
    this.extRows.update((rows) => rows.filter((_, j) => j !== i));
  }

  protected cancel(): void {
    void this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
  }

  protected async submit(): Promise<void> {
    const roles: RoleSpec[] = this.roleRows()
      .filter((r) => r.name.trim())
      .map((r) => ({
        name: r.name.trim(),
        login: r.login,
        createdb: r.createdb,
        createrole: r.createrole,
        ...(r.connectionLimit.trim() !== '' ? { connection_limit: Number(r.connectionLimit) } : {}),
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
    const extensions: ExtensionSpec[] = this.extRows()
      .filter((e) => e.name.trim())
      .map((e) => ({
        name: e.name.trim(),
        database: e.database.trim() || 'postgres',
        ...(e.schema.trim() ? { schema: e.schema.trim() } : {}),
        preload: e.preload,
      }));

    this.busy.set(true);
    this.error.set(null);
    try {
      await this.api.invoke(patchCluster, {
        ns: this.ns(),
        name: this.name(),
        body: { roles, databases, extensions },
      });
      await this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.busy.set(false);
    }
  }
}
