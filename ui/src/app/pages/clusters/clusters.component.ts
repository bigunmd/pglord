import { Component, OnDestroy, computed, inject, signal } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideDatabase, lucideMoon, lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import {
  HlmDialogBody,
  HlmDialogFooter,
  HlmDialogHeader,
  HlmDialogService,
  HlmDialogTitle,
} from '@spartan-ng/helm/dialog';
import { HlmTableImports } from '@spartan-ng/helm/table';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { firstValueFrom } from 'rxjs';

import { Api } from '../../api/api';
import { problemDetail } from '../../core/problem';
import { ClusterSummary } from '../../api/models/cluster-summary';
import { deleteCluster } from '../../api/fn/clusters/delete-cluster';
import { listAllClusters } from '../../api/fn/clusters/list-all-clusters';

const REFRESH_MS = 5000;

/** Confirm destructive delete; resolves true only on explicit confirmation. */
@Component({
  selector: 'pg-cluster-delete-dialog',
  imports: [
    TranslocoModule,
    HlmButton,
    HlmDialogBody,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
  ],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusters.deleteTitle' | transloco: { name: name } }}</h2>
    </div>
    <div hlmDialogBody>
      <p class="text-muted-foreground text-sm">{{ 'clusters.deleteWarning' | transloco }}</p>
      <p class="mt-3 break-all font-mono text-sm">{{ namespace }}/{{ name }}</p>
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn variant="destructive" data-testid="confirm-delete" (click)="ref.close(true)">
        {{ 'clusters.delete' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterDeleteDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  private readonly context = injectBrnDialogContext<{ name: string; namespace: string }>();
  protected readonly name = this.context.name;
  protected readonly namespace = this.context.namespace;
}

@Component({
  selector: 'pg-clusters',
  imports: [TranslocoModule, RouterLink, NgIcon, HlmButton, HlmInput, HlmTableImports],
  providers: [provideIcons({ lucideDatabase, lucideMoon, lucidePlus, lucideTrash2 })],
  template: `
    <div class="pg-page">
      <header class="pg-page-header">
        <div>
          <h1 class="pg-page-title">{{ 'clusters.title' | transloco }}</h1>
          <p class="pg-page-description">{{ 'ux.clusters.description' | transloco }}</p>
        </div>
        <button hlmBtn data-testid="open-create" (click)="openCreate()">
          <ng-icon name="lucidePlus" size="16" aria-hidden="true" />
          {{ 'clusters.create' | transloco }}
        </button>
      </header>

      <section class="pg-panel" [attr.aria-label]="'clusters.title' | transloco">
        <div class="border-border flex flex-wrap items-end gap-4 border-b p-5">
          <div class="min-w-0 flex-1 basis-56">
            <label for="cluster-search" class="mb-2 block text-sm font-medium">{{
              'ux.clusters.search' | transloco
            }}</label>
            <input
              hlmInput
              id="cluster-search"
              type="search"
              class="w-full"
              [placeholder]="'ux.clusters.searchHint' | transloco"
              [value]="query()"
              (input)="search($event)"
            />
          </div>
          <div class="min-w-0 flex-1 basis-44 sm:max-w-64">
            <label for="cluster-namespace" class="mb-2 block text-sm font-medium">{{
              'clusters.namespace' | transloco
            }}</label>
            <select
              id="cluster-namespace"
              class="border-input bg-background h-10 w-full rounded-md border px-3 text-sm"
              data-testid="ns-switcher"
              [value]="selectedNs()"
              (change)="selectNs($event)"
            >
              <option value="">{{ 'clusters.allNamespaces' | transloco }}</option>
              @for (n of namespaces(); track n) {
                <option [value]="n" [selected]="n === selectedNs()">{{ n }}</option>
              }
            </select>
          </div>
          <button hlmBtn variant="outline" [disabled]="refreshing()" (click)="refresh()">
            {{ 'ux.clusters.refresh' | transloco }}
          </button>
        </div>

        @if (error(); as err) {
          <div class="pg-alert m-5" role="alert">
            <p>{{ err }}</p>
            @if (clusters().length > 0) {
              <p class="mt-2 text-sm">{{ 'ux.clusters.stale' | transloco }}</p>
            }
          </div>
        }
        @if (!loaded()) {
          <div class="pg-empty" role="status">{{ 'common.loading' | transloco }}</div>
        } @else if (clusters().length === 0 && !error()) {
          <div class="pg-empty">
            <ng-icon
              name="lucideDatabase"
              size="32"
              class="text-muted-foreground"
              aria-hidden="true"
            />
            <h2 class="mt-4 font-semibold">{{ 'clusters.emptyTitle' | transloco }}</h2>
            <p class="text-muted-foreground mt-2 text-sm">{{ 'clusters.emptyHint' | transloco }}</p>
            <button hlmBtn class="mt-5" (click)="openCreate()">
              {{ 'clusters.create' | transloco }}
            </button>
          </div>
        } @else if (clusters().length > 0 && visible().length === 0) {
          <div class="pg-empty" data-testid="filter-empty">
            <h2 class="font-semibold">{{ 'ux.clusters.noMatches' | transloco }}</h2>
            <p class="text-muted-foreground mt-2 text-sm">
              {{ 'ux.clusters.noMatchesHint' | transloco }}
            </p>
            <button hlmBtn variant="outline" class="mt-5" (click)="clearFilters()">
              {{ 'ux.clusters.clearFilters' | transloco }}
            </button>
          </div>
        } @else if (visible().length > 0) {
          <div class="text-muted-foreground px-5 py-3 text-xs" role="status">
            {{
              'ux.clusters.showing'
                | transloco: { visible: visible().length, total: clusters().length }
            }}
          </div>
          <div class="overflow-x-auto">
            <table hlmTable class="w-full">
              <caption class="sr-only">
                {{
                  'ux.clusters.tableCaption' | transloco
                }}
              </caption>
              <thead hlmTHead>
                <tr hlmTr>
                  <th hlmTh scope="col">{{ 'clusters.name' | transloco }}</th>
                  <th hlmTh scope="col">{{ 'clusters.instances' | transloco }}</th>
                  <th hlmTh scope="col" class="hidden lg:table-cell">
                    {{ 'clusters.image' | transloco }}
                  </th>
                  <th hlmTh scope="col" class="w-12">
                    <span class="sr-only">{{ 'ux.clusters.actions' | transloco }}</span>
                  </th>
                </tr>
              </thead>
              <tbody hlmTBody>
                @for (c of visible(); track c.namespace + '/' + c.name) {
                  <tr hlmTr [attr.data-testid]="'cluster-row-' + c.name">
                    <td hlmTd class="max-w-48 whitespace-normal py-5 sm:max-w-none">
                      <a
                        [routerLink]="['/namespaces', c.namespace, 'clusters', c.name]"
                        class="text-primary break-all font-mono text-sm font-semibold underline-offset-4 hover:underline"
                        >{{ c.name }}</a
                      >
                      <p class="text-muted-foreground mt-1 break-all font-mono text-xs">
                        {{ c.namespace }}
                      </p>
                      <p class="text-muted-foreground mt-2 break-all font-mono text-xs lg:hidden">
                        {{ c.image }}
                      </p>
                    </td>
                    <td hlmTd class="whitespace-normal">
                      @if (c.hibernated) {
                        <span
                          class="pg-status text-muted-foreground"
                          [attr.data-testid]="'hibernated-' + c.name"
                        >
                          <ng-icon name="lucideMoon" size="12" aria-hidden="true" />{{
                            'clusters.hibernated' | transloco
                          }}
                        </span>
                      } @else {
                        <span
                          class="pg-status"
                          [class.text-success]="c.replicas > 0 && c.ready_replicas >= c.replicas"
                          [class.text-warning]="c.ready_replicas < c.replicas || c.replicas === 0"
                        >
                          {{
                            (c.replicas > 0 && c.ready_replicas >= c.replicas
                              ? 'ux.clusters.ready'
                              : 'ux.clusters.notReady'
                            ) | transloco
                          }}
                        </span>
                        <p class="text-muted-foreground mt-2 font-mono text-xs">
                          {{
                            'clusters.readyOf'
                              | transloco: { ready: c.ready_replicas, total: c.replicas }
                          }}
                        </p>
                        <span class="mt-2 inline-flex max-w-40 flex-wrap gap-1" aria-hidden="true">
                          @for (i of range(c.replicas); track i) {
                            <span
                              class="h-1.5 w-3 rounded-sm"
                              [class.bg-success]="i < c.ready_replicas"
                              [class.bg-border]="i >= c.ready_replicas"
                            ></span>
                          }
                        </span>
                      }
                    </td>
                    <td
                      hlmTd
                      class="text-muted-foreground hidden max-w-80 whitespace-normal break-all font-mono text-xs lg:table-cell"
                    >
                      {{ c.image }}
                    </td>
                    <td hlmTd>
                      <button
                        hlmBtn
                        variant="ghost"
                        size="icon-sm"
                        class="text-muted-foreground hover:text-destructive"
                        [attr.aria-label]="
                          'ux.clusters.deleteLabel'
                            | transloco: { name: c.name, namespace: c.namespace }
                        "
                        [attr.data-testid]="'delete-' + c.name"
                        (click)="openDelete(c)"
                      >
                        <ng-icon name="lucideTrash2" size="16" aria-hidden="true" />
                      </button>
                    </td>
                  </tr>
                }
              </tbody>
            </table>
          </div>
        }
      </section>
    </div>
  `,
})
export class ClustersComponent implements OnDestroy {
  private readonly api = inject(Api);
  private readonly dialog = inject(HlmDialogService);
  private readonly router = inject(Router);
  protected readonly clusters = signal<ClusterSummary[]>([]);
  protected readonly loaded = signal(false);
  protected readonly refreshing = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly query = signal('');
  // Empty means all namespaces; preserve the choice even when its last cluster is deleted.
  protected readonly selectedNs = signal(localStorage.getItem('pglord.namespace') ?? '');
  protected readonly namespaces = computed(() =>
    [
      ...new Set([...this.clusters().map((c) => c.namespace), this.selectedNs()].filter(Boolean)),
    ].sort(),
  );
  protected readonly visible = computed(() => {
    const query = this.query().trim().toLocaleLowerCase();
    return this.clusters().filter(
      (c) =>
        (!this.selectedNs() || c.namespace === this.selectedNs()) &&
        (!query ||
          c.name.toLocaleLowerCase().includes(query) ||
          c.namespace.toLocaleLowerCase().includes(query)),
    );
  });

  protected search(event: Event): void {
    this.query.set((event.target as HTMLInputElement).value);
  }

  protected selectNs(event: Event): void {
    const value = (event.target as HTMLSelectElement).value;
    this.selectedNs.set(value);
    localStorage.setItem('pglord.namespace', value);
  }

  protected clearFilters(): void {
    this.query.set('');
    this.selectedNs.set('');
    localStorage.setItem('pglord.namespace', '');
  }

  protected range(n: number): number[] {
    return Array.from({ length: n }, (_, i) => i);
  }

  private readonly timer = setInterval(() => void this.refresh(), REFRESH_MS);
  private pending: Promise<void> | null = null;

  constructor() {
    void this.refresh();
  }
  ngOnDestroy(): void {
    clearInterval(this.timer);
  }

  protected refresh(): Promise<void> {
    if (this.pending) return this.pending;
    this.refreshing.set(true);
    this.pending = this.api
      .invoke(listAllClusters)
      .then((clusters) => {
        this.clusters.set(clusters);
        this.error.set(null);
      })
      .catch((err) => this.error.set(problemDetail(err)))
      .finally(() => {
        this.loaded.set(true);
        this.refreshing.set(false);
        this.pending = null;
      });
    return this.pending;
  }

  openCreate(): void {
    // Never guess a namespace: without a selection the create page asks the server.
    const ns = this.selectedNs();
    void this.router.navigate(['/clusters/new'], ns ? { queryParams: { ns } } : {});
  }

  async openDelete(c: ClusterSummary): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(ClusterDeleteDialogComponent, {
        context: { name: c.name, namespace: c.namespace },
      }).closed$,
    );
    if (!confirmed) return;
    try {
      await this.api.invoke(deleteCluster, { ns: c.namespace, name: c.name });
      // A poll started before deletion may still contain the deleted cluster.
      if (this.pending) await this.pending;
      await this.refresh();
    } catch (err) {
      this.error.set(problemDetail(err));
    }
  }
}
