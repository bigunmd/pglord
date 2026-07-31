import { Component, OnDestroy, inject, input, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import {
  lucideArrowLeft,
  lucideCrown,
  lucidePencil,
  lucidePower,
  lucideRefreshCw,
  lucideServer,
  lucideSettings,
  lucideTriangleAlert,
} from '@ng-icons/lucide';
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
import { GucInfo } from '../../api/models/guc-info';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { listGucs } from '../../api/fn/system/list-gucs';
import { patchCluster } from '../../api/fn/clusters/patch-cluster';
import { restartCluster } from '../../api/fn/clusters/restart-cluster';
import { switchover } from '../../api/fn/clusters/switchover';
import { WorkloadSpec } from '../../api/models/workload-spec';
import { ClusterConfigDialogComponent } from './cluster-config-dialog.component';
import { ClusterEditDialogComponent } from './cluster-edit-dialog.component';
import { ClusterWorkloadDialogComponent } from './cluster-workload-dialog.component';

const REFRESH_MS = 5000;

/** Editors hold forms and lists; the default dialog (sm:max-w-sm, p-4) is too
 * tight for them. Confirmations keep the narrow default. */
export const EDITOR_DIALOG_CLASS = 'sm:max-w-2xl p-6 gap-6';
/** Short forms: roomier than the default, narrower than the list editors. */
export const FORM_DIALOG_CLASS = 'sm:max-w-lg p-6 gap-6';

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

/** Confirm a rolling restart. */
@Component({
  selector: 'pg-restart-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'config.restartTitle' | transloco: { name: name } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">{{ 'config.restartWarning' | transloco }}</p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-restart" (click)="ref.close(true)">
        {{ 'config.restart' | transloco }}
      </button>
    </div>
  `,
})
export class RestartDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  protected readonly name = injectBrnDialogContext<{ name: string }>().name;
}

@Component({
  selector: 'pg-cluster-detail',
  imports: [RouterLink, TranslocoModule, NgIcon, HlmButton, HlmTableImports],
  providers: [
    provideIcons({
      lucideArrowLeft,
      lucideCrown,
      lucidePencil,
      lucidePower,
      lucideRefreshCw,
      lucideServer,
      lucideSettings,
      lucideTriangleAlert,
    }),
  ],
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

        <!-- Placement and resources -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'workload.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-workload"
            [disabled]="busy()"
            (click)="openWorkload(d)"
          >
            <ng-icon name="lucideServer" size="14" />
            {{ 'workload.edit' | transloco }}
          </button>
        </div>
        <dl class="text-muted-foreground mt-2 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt>{{ 'workload.antiAffinity' | transloco }}</dt>
          <dd class="text-foreground" data-testid="workload-anti-affinity">
            {{ 'workload.antiAffinityModes.' + (d.workload.anti_affinity ?? 'preferred') | transloco }}
          </dd>
          <dt>{{ 'workload.resources' | transloco }}</dt>
          <dd class="text-foreground font-mono text-xs" data-testid="workload-resources">
            {{ resourceSummary(d.workload) }}
          </dd>
          @if (labelSummary(d.workload); as l) {
            <dt>{{ 'workload.labels' | transloco }}</dt>
            <dd class="text-foreground font-mono text-xs">{{ l }}</dd>
          }
          @if (annotationSummary(d.workload); as a) {
            <dt>{{ 'workload.annotations' | transloco }}</dt>
            <dd class="text-foreground font-mono text-xs">{{ a }}</dd>
          }
        </dl>

        <!-- Configuration -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'config.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-config"
            [disabled]="busy()"
            (click)="openConfig(d)"
          >
            <ng-icon name="lucideSettings" size="14" />
            {{ 'config.edit' | transloco }}
          </button>
        </div>

        @if (d.pending_restart) {
          <div
            class="border-warning mt-2 flex items-center gap-2 rounded-lg border p-3 text-sm"
            data-testid="restart-banner"
          >
            <ng-icon name="lucideTriangleAlert" size="18" class="text-warning" />
            <span>{{ 'config.pendingRestart' | transloco }}</span>
            <button
              hlmBtn
              size="sm"
              class="ml-auto"
              data-testid="open-restart"
              [disabled]="busy()"
              (click)="openRestart()"
            >
              <ng-icon name="lucidePower" size="14" />
              {{ 'config.restart' | transloco }}
            </button>
          </div>
        }

        @if (parameterRows(d).length) {
          <div class="border-border mt-2 overflow-x-auto rounded-lg border">
            <table hlmTable class="w-full">
              <thead hlmTHead>
                <tr hlmTr>
                  <th hlmTh>{{ 'config.setting' | transloco }}</th>
                  <th hlmTh>{{ 'config.value' | transloco }}</th>
                </tr>
              </thead>
              <tbody hlmTBody>
                @for (p of parameterRows(d); track p.name) {
                  <tr hlmTr [attr.data-testid]="'config-row-' + p.name">
                    <td hlmTd class="font-mono text-xs">{{ p.name }}</td>
                    <td hlmTd class="font-mono text-xs">{{ p.value }}</td>
                  </tr>
                }
              </tbody>
            </table>
          </div>
        } @else {
          <p class="text-muted-foreground mt-2 text-sm">{{ 'config.usingDefaults' | transloco }}</p>
        }
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

  protected parameterRows(d: ClusterDetail): { name: string; value: string }[] {
    return Object.entries(d.parameters ?? {})
      .map(([name, value]) => ({ name, value }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  protected resourceSummary(w: WorkloadSpec): string {
    const r = w.resources ?? {};
    const parts = [
      r.cpu_request ? `cpu ${r.cpu_request}` : null,
      r.cpu_limit ? `cpu limit ${r.cpu_limit}` : null,
      r.memory_request ? `mem ${r.memory_request}` : null,
      r.memory_limit ? `mem limit ${r.memory_limit}` : null,
    ].filter(Boolean);
    return parts.length ? parts.join(' · ') : '—';
  }

  protected labelSummary(w: WorkloadSpec): string {
    return summarizeMap(w.labels);
  }

  protected annotationSummary(w: WorkloadSpec): string {
    return summarizeMap(w.annotations);
  }

  async openWorkload(d: ClusterDetail): Promise<void> {
    const workload = await firstValueFrom(
      this.dialog.open<WorkloadSpec | undefined>(ClusterWorkloadDialogComponent, {
        context: { workload: d.workload },
        contentClass: EDITOR_DIALOG_CLASS,
      }).closed$,
    );
    if (!workload) {
      return;
    }
    await this.run(() => this.api.invoke(patchCluster, { name: this.name(), body: { workload } }));
  }

  async openConfig(d: ClusterDetail): Promise<void> {
    let catalog: GucInfo[];
    try {
      catalog = await this.api.invoke(listGucs);
    } catch {
      this.error.set('Could not load the settings catalog.');
      return;
    }
    const parameters = await firstValueFrom(
      this.dialog.open<Record<string, string> | undefined>(ClusterConfigDialogComponent, {
        context: { parameters: d.parameters ?? {}, catalog },
        contentClass: EDITOR_DIALOG_CLASS,
      }).closed$,
    );
    if (!parameters) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { name: this.name(), body: { parameters } }),
    );
  }

  async openRestart(): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(RestartDialogComponent, { context: { name: this.name() } })
        .closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() => this.api.invoke(restartCluster, { name: this.name() }));
  }

  async openEdit(d: ClusterDetail): Promise<void> {
    const change = await firstValueFrom(
      this.dialog.open<SpecChange | undefined>(ClusterEditDialogComponent, {
        context: { replicas: d.replicas, storageSize: d.storage_size },
        contentClass: FORM_DIALOG_CLASS,
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

function summarizeMap(map: Record<string, string> | undefined): string {
  const entries = Object.entries(map ?? {});
  return entries.map(([k, v]) => `${k}=${v}`).join(' · ');
}
