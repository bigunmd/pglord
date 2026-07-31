import { Component, OnDestroy, inject, input, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideArrowLeft, lucideCrown, lucidePencil, lucideRefreshCw } from '@ng-icons/lucide';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import {
  HlmDialogFooter,
  HlmDialogHeader,
  HlmDialogService,
  HlmDialogTitle,
} from '@spartan-ng/helm/dialog';
import { HlmTableImports } from '@spartan-ng/helm/table';
import { firstValueFrom } from 'rxjs';

import { Api } from '../../api/api';
import { ClusterDetail } from '../../api/models/cluster-detail';
import { SpecChange } from '../../api/models/spec-change';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { patchCluster } from '../../api/fn/clusters/patch-cluster';
import { switchover } from '../../api/fn/clusters/switchover';
import { ClusterEditDialogComponent } from './cluster-edit-dialog.component';

const REFRESH_MS = 5000;

/** Confirm a planned switchover; resolves true only on explicit confirmation. */
@Component({
  selector: 'pg-switchover-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusterDetail.switchoverTitle' | transloco: { pod: pod } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">
      {{ 'clusterDetail.switchoverWarning' | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-switchover" (click)="ref.close(true)">
        {{ 'clusterDetail.makePrimary' | transloco }}
      </button>
    </div>
  `,
})
export class SwitchoverDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  protected readonly pod = injectBrnDialogContext<{ pod: string }>().pod;
}

@Component({
  selector: 'pg-cluster-detail',
  imports: [RouterLink, TranslocoModule, NgIcon, HlmButton, HlmTableImports],
  providers: [provideIcons({ lucideArrowLeft, lucideCrown, lucidePencil, lucideRefreshCw })],
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
          <button
            hlmBtn
            variant="outline"
            size="sm"
            class="ml-auto"
            data-testid="open-edit"
            [disabled]="busy()"
            (click)="openEdit(d)"
          >
            <ng-icon name="lucidePencil" size="14" />
            {{ 'clusterDetail.edit' | transloco }}
          </button>
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
                <th hlmTh class="w-32"></th>
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
                  <td hlmTd>
                    @if (i.pod !== d.primary && i.ready && i.role === 'standby') {
                      <button
                        hlmBtn
                        variant="ghost"
                        size="sm"
                        [disabled]="busy()"
                        [attr.data-testid]="'switchover-' + i.pod"
                        (click)="openSwitchover(d, i.pod)"
                      >
                        <ng-icon name="lucideRefreshCw" size="14" />
                        {{ 'clusterDetail.makePrimary' | transloco }}
                      </button>
                    }
                  </td>
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
  private readonly dialog = inject(HlmDialogService);

  readonly name = input.required<string>();

  protected readonly detail = signal<ClusterDetail | null>(null);
  protected readonly error = signal<string | null>(null);
  /** Blocks concurrent operations so a double-click cannot fire twice. */
  protected readonly busy = signal(false);

  async openEdit(d: ClusterDetail): Promise<void> {
    const change = await firstValueFrom(
      this.dialog.open<SpecChange | undefined>(ClusterEditDialogComponent, {
        context: { replicas: d.replicas, storageSize: d.storage_size },
      }).closed$,
    );
    if (!change) {
      return;
    }
    await this.run(() => this.api.invoke(patchCluster, { name: this.name(), body: change }));
  }

  async openSwitchover(d: ClusterDetail, pod: string): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(SwitchoverDialogComponent, { context: { pod } }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() =>
      this.api.invoke(switchover, { name: this.name(), body: { target: pod } }),
    );
  }

  private async run(op: () => Promise<unknown>): Promise<void> {
    this.busy.set(true);
    try {
      await op();
      this.error.set(null);
      await this.refresh();
    } catch (err) {
      const e = err as { error?: { detail?: string; code?: string }; message?: string };
      this.error.set(e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed');
    } finally {
      this.busy.set(false);
    }
  }

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
