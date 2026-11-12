import { Component, OnDestroy, computed, inject, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButton } from '@spartan-ng/helm/button';

import { Api } from '../../api/api';
import { listAllClusters } from '../../api/fn/clusters/list-all-clusters';
import { version } from '../../api/fn/system/version';
import { ClusterSummary } from '../../api/models/cluster-summary';
import { problemDetail } from '../../core/problem';

type ServerStatus = 'checking' | 'reachable' | 'unreachable';

@Component({
  selector: 'pg-home',
  imports: [TranslocoModule, RouterLink, HlmButton],
  template: `
    <div class="pg-page">
      <header class="pg-page-header">
        <div>
          <h1 class="pg-page-title">{{ 'ux.overview.title' | transloco }}</h1>
          <p class="pg-page-description">{{ 'ux.overview.description' | transloco }}</p>
        </div>
        <a hlmBtn routerLink="/clusters/new">{{ 'clusters.create' | transloco }}</a>
      </header>

      <section
        class="pg-panel mb-6 flex flex-wrap items-center justify-between gap-4 p-5"
        aria-labelledby="server-title"
      >
        <div>
          <h2 id="server-title" class="text-sm font-semibold">
            {{ 'home.serverStatus' | transloco }}
          </h2>
          <p class="text-muted-foreground mt-1 text-sm">
            {{ 'ux.overview.serverHint' | transloco }}
          </p>
        </div>
        <div class="flex flex-wrap items-center gap-3">
          @if (serverVersion(); as v) {
            <span class="text-muted-foreground text-xs"
              >{{ 'home.version' | transloco }} <span class="font-mono">{{ v }}</span></span
            >
          }
          <span
            class="pg-status"
            [class.text-success]="status() === 'reachable'"
            [class.text-destructive]="status() === 'unreachable'"
            role="status"
          >
            {{ 'home.' + status() | transloco }}
          </span>
          <button hlmBtn variant="outline" size="sm" [disabled]="refreshing()" (click)="refresh()">
            {{ 'ux.overview.refresh' | transloco }}
          </button>
        </div>
      </section>

      @if (clusterError(); as err) {
        <div class="pg-alert mb-6" role="alert">
          <p class="font-medium">{{ 'ux.overview.loadError' | transloco }}</p>
          <p class="mt-1 text-sm">{{ err }}</p>
          @if (loaded()) {
            <p class="mt-2 text-sm">{{ 'ux.overview.stale' | transloco }}</p>
          }
        </div>
      }
      @if (!loaded() && !clusterError()) {
        <div class="pg-empty" role="status">{{ 'common.loading' | transloco }}</div>
      } @else if (loaded()) {
        <dl class="pg-panel mb-6 grid grid-cols-2 divide-x divide-border lg:grid-cols-4">
          <div class="p-5">
            <dt class="text-muted-foreground text-sm">{{ 'ux.overview.total' | transloco }}</dt>
            <dd class="mt-2 text-3xl font-semibold tabular-nums">{{ totals().total }}</dd>
          </div>
          <div class="p-5">
            <dt class="text-muted-foreground text-sm">{{ 'ux.overview.ready' | transloco }}</dt>
            <dd class="mt-2 text-3xl font-semibold tabular-nums">{{ totals().ready }}</dd>
          </div>
          <div class="p-5">
            <dt class="text-muted-foreground text-sm">{{ 'ux.overview.attention' | transloco }}</dt>
            <dd
              class="mt-2 text-3xl font-semibold tabular-nums"
              [class.text-warning]="totals().attention > 0"
            >
              {{ totals().attention }}
            </dd>
          </div>
          <div class="p-5">
            <dt class="text-muted-foreground text-sm">{{ 'clusters.hibernated' | transloco }}</dt>
            <dd class="mt-2 text-3xl font-semibold tabular-nums">{{ totals().hibernated }}</dd>
          </div>
        </dl>
        <div class="grid items-start gap-6 lg:grid-cols-[minmax(0,2fr)_minmax(16rem,1fr)]">
          <section class="pg-panel min-w-0" aria-labelledby="attention-title">
            <div class="pg-section-header flex flex-wrap items-center justify-between gap-3">
              <h2 id="attention-title" class="font-semibold">
                {{ 'ux.overview.attention' | transloco }}
              </h2>
              <a
                routerLink="/clusters"
                class="text-primary text-sm font-medium underline-offset-4 hover:underline"
                >{{ 'ux.overview.viewClusters' | transloco }}</a
              >
            </div>
            @if (clusters().length === 0) {
              <div class="pg-empty">
                <h3 class="font-semibold">{{ 'clusters.emptyTitle' | transloco }}</h3>
                <p class="text-muted-foreground mt-2 text-sm">
                  {{ 'clusters.emptyHint' | transloco }}
                </p>
                <a hlmBtn routerLink="/clusters/new" class="mt-5">{{
                  'clusters.create' | transloco
                }}</a>
              </div>
            } @else if (attentionClusters().length === 0) {
              <div class="pg-empty">
                <p class="font-semibold">{{ 'ux.overview.noAttention' | transloco }}</p>
                <p class="text-muted-foreground mt-2 text-sm">
                  {{ 'ux.overview.readinessHint' | transloco }}
                </p>
              </div>
            } @else {
              <p class="text-muted-foreground px-5 pt-4 text-sm">
                {{ 'ux.overview.attentionHint' | transloco }}
              </p>
              <ul class="divide-border divide-y p-5">
                @for (c of attentionClusters(); track c.namespace + '/' + c.name) {
                  <li class="flex flex-wrap items-center justify-between gap-3 py-4">
                    <div class="min-w-0">
                      <a
                        [routerLink]="['/namespaces', c.namespace, 'clusters', c.name]"
                        class="text-primary break-all font-mono text-sm font-semibold underline-offset-4 hover:underline"
                        >{{ c.name }}</a
                      >
                      <p class="text-muted-foreground mt-1 break-all font-mono text-xs">
                        {{ c.namespace }}
                      </p>
                    </div>
                    <span class="pg-status text-warning">{{
                      'clusters.readyOf' | transloco: { ready: c.ready_replicas, total: c.replicas }
                    }}</span>
                  </li>
                }
              </ul>
            }
          </section>
          <aside class="pg-panel p-5" aria-labelledby="actions-title">
            <h2 id="actions-title" class="font-semibold">
              {{ 'ux.overview.actions' | transloco }}
            </h2>
            <div class="mt-5 space-y-5">
              <div>
                <a
                  routerLink="/clusters"
                  class="text-primary text-sm font-semibold underline-offset-4 hover:underline"
                  >{{ 'ux.overview.viewClusters' | transloco }}</a
                >
                <p class="text-muted-foreground mt-1 text-sm">
                  {{ 'ux.overview.clustersHint' | transloco }}
                </p>
              </div>
              <div>
                <a
                  routerLink="/preflight"
                  class="text-primary text-sm font-semibold underline-offset-4 hover:underline"
                  >{{ 'ux.overview.checkPermissions' | transloco }}</a
                >
                <p class="text-muted-foreground mt-1 text-sm">
                  {{ 'ux.overview.permissionsHint' | transloco }}
                </p>
              </div>
            </div>
          </aside>
        </div>
      }
    </div>
  `,
})
export class HomeComponent implements OnDestroy {
  private readonly api = inject(Api);
  protected readonly status = signal<ServerStatus>('checking');
  protected readonly serverVersion = signal<string | null>(null);
  protected readonly clusters = signal<ClusterSummary[]>([]);
  protected readonly clusterError = signal<string | null>(null);
  protected readonly loaded = signal(false);
  protected readonly refreshing = signal(false);
  protected readonly attentionClusters = computed(() =>
    this.clusters().filter(
      (c) => !c.hibernated && (c.replicas === 0 || c.ready_replicas < c.replicas),
    ),
  );
  protected readonly totals = computed(() => {
    const all = this.clusters();
    const hibernated = all.filter((c) => c.hibernated).length;
    const attention = this.attentionClusters().length;
    return { total: all.length, ready: all.length - hibernated - attention, attention, hibernated };
  });
  private readonly timer = setInterval(() => void this.refresh(), 10000);
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
    this.pending = Promise.all([
      this.api
        .invoke(version)
        .then((v) => {
          this.serverVersion.set(v.version);
          this.status.set('reachable');
        })
        .catch(() => {
          this.serverVersion.set(null);
          this.status.set('unreachable');
        }),
      this.api
        .invoke(listAllClusters)
        .then((clusters) => {
          this.clusters.set(clusters);
          this.loaded.set(true);
          this.clusterError.set(null);
        })
        .catch((err) => this.clusterError.set(problemDetail(err))),
    ])
      .then(() => undefined)
      .finally(() => {
        this.refreshing.set(false);
        this.pending = null;
      });
    return this.pending;
  }
}
