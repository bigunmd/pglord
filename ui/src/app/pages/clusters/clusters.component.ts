import { Component, OnDestroy, inject, signal } from '@angular/core';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideDatabase, lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogService, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmTableImports } from '@spartan-ng/helm/table';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { firstValueFrom } from 'rxjs';

import { Api } from '../../api/api';
import { ClusterSpec } from '../../api/models/cluster-spec';
import { ClusterSummary } from '../../api/models/cluster-summary';
import { createCluster } from '../../api/fn/clusters/create-cluster';
import { deleteCluster } from '../../api/fn/clusters/delete-cluster';
import { listClusters } from '../../api/fn/clusters/list-clusters';
import { ClusterCreateDialogComponent } from './cluster-create-dialog.component';

const REFRESH_MS = 5000;

/** Confirm destructive delete; resolves true only on explicit confirmation. */
@Component({
  selector: 'pg-cluster-delete-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusters.deleteTitle' | transloco: { name: name } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">{{ 'clusters.deleteWarning' | transloco }}</p>
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
  protected readonly name = injectBrnDialogContext<{ name: string }>().name;
}

@Component({
  selector: 'pg-clusters',
  imports: [TranslocoModule, NgIcon, HlmButton, HlmTableImports],
  providers: [provideIcons({ lucideDatabase, lucidePlus, lucideTrash2 })],
  template: `
    <div class="mx-auto max-w-4xl">
      <div class="mb-4 flex items-center justify-between">
        <h1 class="text-lg font-semibold">{{ 'clusters.title' | transloco }}</h1>
        <button hlmBtn size="sm" data-testid="open-create" (click)="openCreate()">
          <ng-icon name="lucidePlus" size="16" />
          {{ 'clusters.create' | transloco }}
        </button>
      </div>

      @if (error(); as err) {
        <p class="text-destructive mb-3 text-sm" role="alert">{{ err }}</p>
      }

      @if (loaded() && clusters().length === 0) {
        <div class="border-border rounded-lg border border-dashed p-10 text-center">
          <ng-icon name="lucideDatabase" size="28" class="text-muted-foreground" />
          <p class="mt-2 font-medium">{{ 'clusters.emptyTitle' | transloco }}</p>
          <p class="text-muted-foreground mt-1 text-sm">{{ 'clusters.emptyHint' | transloco }}</p>
        </div>
      } @else {
        <div class="border-border overflow-x-auto rounded-lg border">
          <table hlmTable class="w-full">
            <thead hlmTHead>
              <tr hlmTr>
                <th hlmTh>{{ 'clusters.name' | transloco }}</th>
                <th hlmTh>{{ 'clusters.instances' | transloco }}</th>
                <th hlmTh>{{ 'clusters.image' | transloco }}</th>
                <th hlmTh class="w-12"></th>
              </tr>
            </thead>
            <tbody hlmTBody>
              @for (c of clusters(); track c.name) {
                <tr
                  hlmTr
                  class="hover:bg-muted/50 cursor-pointer transition-colors"
                  [attr.data-testid]="'cluster-row-' + c.name"
                  (click)="openDetail(c.name)"
                >
                  <td hlmTd class="font-mono text-sm font-medium">{{ c.name }}</td>
                  <td hlmTd>
                    <span class="inline-flex items-center gap-2">
                      <!-- Health strip: one dot per instance, filled = ready. -->
                      <span
                        class="inline-flex gap-1"
                        role="img"
                        [attr.aria-label]="
                          'clusters.readyOf' | transloco: { ready: c.ready_replicas, total: c.replicas }
                        "
                      >
                        @for (i of range(c.replicas); track i) {
                          <span
                            class="h-2.5 w-2.5 rounded-full transition-colors"
                            [class.bg-success]="i < c.ready_replicas"
                            [class.bg-border]="i >= c.ready_replicas"
                          ></span>
                        }
                      </span>
                      <span class="text-muted-foreground font-mono text-xs tabular-nums">
                        {{ c.ready_replicas }}/{{ c.replicas }}
                      </span>
                    </span>
                  </td>
                  <td hlmTd class="text-muted-foreground font-mono text-xs">{{ c.image }}</td>
                  <td hlmTd>
                    <button
                      hlmBtn
                      variant="ghost"
                      size="icon-sm"
                      class="text-muted-foreground hover:text-destructive"
                      [attr.aria-label]="'clusters.delete' | transloco"
                      [attr.data-testid]="'delete-' + c.name"
                      (click)="$event.stopPropagation(); openDelete(c)"
                    >
                      <ng-icon name="lucideTrash2" size="16" />
                    </button>
                  </td>
                </tr>
              }
              @if (!loaded()) {
                <tr hlmTr>
                  <td hlmTd colspan="4" class="text-muted-foreground py-6 text-center text-sm">
                    {{ 'common.loading' | transloco }}
                  </td>
                </tr>
              }
            </tbody>
          </table>
        </div>
      }
    </div>
  `,
})
export class ClustersComponent implements OnDestroy {
  private readonly api = inject(Api);
  private readonly dialog = inject(HlmDialogService);
  private readonly router = inject(Router);

  openDetail(name: string): void {
    void this.router.navigate(['/clusters', name]);
  }

  protected readonly clusters = signal<ClusterSummary[]>([]);
  protected readonly loaded = signal(false);
  protected readonly error = signal<string | null>(null);

  protected range(n: number): number[] {
    return Array.from({ length: n }, (_, i) => i);
  }

  private readonly timer = setInterval(() => void this.refresh(), REFRESH_MS);

  constructor() {
    void this.refresh();
  }

  ngOnDestroy(): void {
    clearInterval(this.timer);
  }

  protected async refresh(): Promise<void> {
    try {
      this.clusters.set(await this.api.invoke(listClusters));
      this.error.set(null);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.loaded.set(true);
    }
  }

  async openCreate(): Promise<void> {
    const spec = await firstValueFrom(
      this.dialog.open<ClusterSpec | undefined>(ClusterCreateDialogComponent, {
        contentClass: 'sm:max-w-lg p-6 gap-6',
      }).closed$,
    );
    if (!spec) {
      return;
    }
    try {
      await this.api.invoke(createCluster, { body: spec });
      await this.refresh();
    } catch (err) {
      this.error.set(problemDetail(err));
    }
  }

  async openDelete(c: ClusterSummary): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(ClusterDeleteDialogComponent, { context: { name: c.name } })
        .closed$,
    );
    if (!confirmed) {
      return;
    }
    try {
      await this.api.invoke(deleteCluster, { name: c.name });
      await this.refresh();
    } catch (err) {
      this.error.set(problemDetail(err));
    }
  }
}

/** Surface the problem+json detail (stable server contract) or a fallback. */
function problemDetail(err: unknown): string {
  const e = err as { error?: { detail?: string; code?: string }; message?: string };
  return e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed';
}
