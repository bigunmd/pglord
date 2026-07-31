import { Component, OnDestroy, inject, input, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideArrowLeft, lucideCrown } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmTableImports } from '@spartan-ng/helm/table';

import { Api } from '../../api/api';
import { ClusterDetail } from '../../api/models/cluster-detail';
import { getCluster } from '../../api/fn/clusters/get-cluster';

const REFRESH_MS = 5000;

@Component({
  selector: 'pg-cluster-detail',
  imports: [RouterLink, TranslocoModule, NgIcon, HlmButton, HlmTableImports],
  providers: [provideIcons({ lucideArrowLeft, lucideCrown })],
  template: `
    <div class="mx-auto max-w-4xl">
      <a hlmBtn variant="ghost" size="sm" routerLink="/clusters" class="mb-3 -ml-2">
        <ng-icon name="lucideArrowLeft" size="16" />
        {{ 'clusterDetail.back' | transloco }}
      </a>

      @if (error(); as err) {
        <p class="text-destructive mb-3 text-sm" role="alert">{{ err }}</p>
      }

      @if (detail(); as d) {
        <div class="mb-1 flex items-center gap-3">
          <h1 class="font-mono text-lg font-semibold">{{ d.name }}</h1>
          <span class="text-muted-foreground font-mono text-xs tabular-nums">
            {{ d.ready_replicas }}/{{ d.replicas }}
          </span>
        </div>
        <p class="text-muted-foreground mb-4 text-sm">
          <span class="font-mono text-xs">{{ d.image }}</span>
          · {{ d.storage_size }} · {{ 'clusterDetail.primaryIs' | transloco: { pod: d.primary } }}
        </p>

        <div class="border-border overflow-x-auto rounded-lg border">
          <table hlmTable class="w-full">
            <thead hlmTHead>
              <tr hlmTr>
                <th hlmTh>{{ 'clusterDetail.instance' | transloco }}</th>
                <th hlmTh>{{ 'clusterDetail.role' | transloco }}</th>
                <th hlmTh>{{ 'clusterDetail.health' | transloco }}</th>
                <th hlmTh>{{ 'clusterDetail.lsn' | transloco }}</th>
              </tr>
            </thead>
            <tbody hlmTBody>
              @for (i of d.instances; track i.pod) {
                <tr hlmTr [attr.data-testid]="'instance-' + i.pod">
                  <td hlmTd class="font-mono text-sm font-medium">
                    <span class="inline-flex items-center gap-1.5">
                      {{ i.pod }}
                      @if (i.pod === d.primary) {
                        <ng-icon
                          name="lucideCrown"
                          size="14"
                          class="text-primary"
                          [attr.aria-label]="'clusterDetail.recordedPrimary' | transloco"
                        />
                      }
                    </span>
                  </td>
                  <td hlmTd>
                    <span
                      class="rounded-full px-2 py-0.5 text-xs font-medium"
                      [class.bg-primary]="i.role === 'primary'"
                      [class.text-primary-foreground]="i.role === 'primary'"
                      [class.bg-secondary]="i.role === 'standby'"
                      [class.text-secondary-foreground]="i.role === 'standby'"
                      [class.bg-warning]="i.role === 'unknown'"
                      [class.text-background]="i.role === 'unknown'"
                    >
                      {{ 'clusterDetail.roles.' + i.role | transloco }}
                    </span>
                  </td>
                  <td hlmTd>
                    <span class="inline-flex items-center gap-1.5 text-sm">
                      <span
                        class="h-2.5 w-2.5 rounded-full"
                        [class.bg-success]="i.ready"
                        [class.bg-destructive]="!i.ready"
                      ></span>
                      {{ (i.ready ? 'clusterDetail.ready' : 'clusterDetail.notReady') | transloco }}
                    </span>
                  </td>
                  <td hlmTd class="font-mono text-xs tabular-nums">{{ i.lsn ?? '—' }}</td>
                </tr>
              }
            </tbody>
          </table>
        </div>
      } @else if (!error()) {
        <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
      }
    </div>
  `,
})
export class ClusterDetailComponent implements OnDestroy {
  private readonly api = inject(Api);

  readonly name = input.required<string>();

  protected readonly detail = signal<ClusterDetail | null>(null);
  protected readonly error = signal<string | null>(null);

  private readonly timer = setInterval(() => void this.refresh(), REFRESH_MS);

  constructor() {
    queueMicrotask(() => void this.refresh());
  }

  ngOnDestroy(): void {
    clearInterval(this.timer);
  }

  private async refresh(): Promise<void> {
    try {
      this.detail.set(await this.api.invoke(getCluster, { name: this.name() }));
      this.error.set(null);
    } catch (err) {
      const e = err as { error?: { detail?: string; code?: string }; message?: string };
      this.error.set(e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed');
    }
  }
}
