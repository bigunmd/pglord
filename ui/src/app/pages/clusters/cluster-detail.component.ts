import { Component, OnDestroy, computed, inject, input, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { Router, RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import {
  lucideArrowLeft,
  lucideBan,
  lucideCrown,
  lucideLock,
  lucideMoon,
  lucideNetwork,
  lucidePencil,
  lucidePlay,
  lucideUsers,
  lucidePower,
  lucideRefreshCw,
  lucideDatabaseBackup,
  lucideServer,
  lucideSettings,
  lucideShield,
  lucideSun,
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
import { HlmDropdownMenuImports } from '@spartan-ng/helm/dropdown-menu';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';
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
import { hibernate } from '../../api/fn/clusters/hibernate';
import { wake } from '../../api/fn/clusters/wake';
import { fenceInstance } from '../../api/fn/clusters/fence-instance';
import { unfenceInstance } from '../../api/fn/clusters/unfence-instance';
import { rotateCredentials } from '../../api/fn/clusters/rotate-credentials';
import { switchover } from '../../api/fn/clusters/switchover';
import { WorkloadSpec } from '../../api/models/workload-spec';
import { BackupSpec } from '../../api/models/backup-spec';
import { BackupList } from '../../api/models/backup-list';
import { BackupRun } from '../../api/models/backup-run';
import { listBackups } from '../../api/fn/backups/list-backups';
import { createBackup } from '../../api/fn/backups/create-backup';
import { ClusterBackupDialogComponent } from './cluster-backup-dialog.component';
import { ClusterRestoreDialogComponent } from './cluster-restore-dialog.component';
import {
  ClusterEnvDialogComponent,
  EnvDialogResult,
} from './cluster-env-dialog.component';
import { ResourceList } from '../../api/models/resource-list';
import { listRequiredSecrets } from '../../api/fn/clusters/list-required-secrets';
import { RestoreRequest } from '../../api/models/restore-request';
import { restoreCluster } from '../../api/fn/backups/restore-cluster';
import { ClusterConfigDialogComponent } from './cluster-config-dialog.component';
import { ClusterEditDialogComponent } from './cluster-edit-dialog.component';
import { ClusterHbaDialogComponent } from './cluster-hba-dialog.component';
import { HbaRule } from '../../api/models/hba-rule';
import { ClusterWorkloadDialogComponent } from './cluster-workload-dialog.component';
import { ClusterTlsDialogComponent } from './cluster-tls-dialog.component';
import { TlsSpec } from '../../api/models/tls-spec';
import { ClusterPoolerDialogComponent } from './cluster-pooler-dialog.component';
import { PoolerSpec } from '../../api/models/pooler-spec';
import {
  ClusterProvisionDialogComponent,
  ProvisionResult,
} from './cluster-provision-dialog.component';

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

/** Confirm hibernation: every instance stops and connections drop. */
@Component({
  selector: 'pg-hibernate-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusterDetail.hibernateTitle' | transloco: { name: name } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">
      {{ 'clusterDetail.hibernateWarning' | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-hibernate" (click)="ref.close(true)">
        {{ 'clusterDetail.hibernate' | transloco }}
      </button>
    </div>
  `,
})
export class HibernateDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  protected readonly name = injectBrnDialogContext<{ name: string }>().name;
}

/** Confirm fencing one instance: postgres stops, the pod stays. */
@Component({
  selector: 'pg-fence-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'clusterDetail.fenceTitle' | transloco: { pod: pod } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">
      {{ 'clusterDetail.fenceWarning' | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-fence" (click)="ref.close(true)">
        {{ 'clusterDetail.fence' | transloco }}
      </button>
    </div>
  `,
})
export class FenceDialogComponent {
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
  selector: 'pg-rotate-credentials-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'secrets.rotateTitle' | transloco: { name: name } }}</h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">{{ 'secrets.rotateWarning' | transloco }}</p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-rotate-credentials" (click)="ref.close(true)">
        {{ 'secrets.rotate' | transloco }}
      </button>
    </div>
  `,
})
export class RotateCredentialsDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  protected readonly name = injectBrnDialogContext<{ name: string }>().name;
}

/** Confirm toggling synchronous replication; the warning states the
 * strict-blocking trade-off plainly. */
@Component({
  selector: 'pg-sync-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>
        {{ (enable ? 'sync.enableTitle' : 'sync.disableTitle') | transloco: { name: name } }}
      </h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">
      {{ (enable ? 'sync.enableWarning' : 'sync.disableWarning') | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-sync" (click)="ref.close(true)">
        {{ (enable ? 'sync.enable' : 'sync.disable') | transloco }}
      </button>
    </div>
  `,
})
export class SyncDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string; enable: boolean }>();
  protected readonly name = this.ctx.name;
  protected readonly enable = this.ctx.enable;
}

/** Change the instance image: a rolling upgrade, standbys first, primary
 * last. Resolves to the new image, or undefined on cancel. */
@Component({
  selector: 'pg-image-dialog',
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
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'image.title' | transloco: { name: name } }}</h2>
    </div>
    <div class="grid gap-2 py-1">
      <label hlmLabel for="pg-image-input">{{ 'image.label' | transloco }}</label>
      <input
        hlmInput
        id="pg-image-input"
        class="font-mono"
        data-testid="image-input"
        [ngModel]="image()"
        (ngModelChange)="image.set($event)"
      />
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'image.warning' | transloco }}
      </p>
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button
        hlmBtn
        data-testid="confirm-image"
        [disabled]="!canSave()"
        (click)="ref.close(image().trim())"
      >
        {{ 'image.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ImageDialogComponent {
  protected readonly ref = inject<BrnDialogRef<string | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string; image: string }>();
  protected readonly name = this.ctx.name;
  protected readonly image = signal(this.ctx.image);
  protected readonly canSave = computed(
    () => this.image().trim().length > 0 && this.image().trim() !== this.ctx.image,
  );
}

@Component({
  selector: 'pg-cluster-detail',
  imports: [
    RouterLink,
    TranslocoModule,
    NgIcon,
    HlmButton,
    HlmTableImports,
    HlmDropdownMenuImports,
  ],
  providers: [
    provideIcons({
      lucideArrowLeft,
      lucideBan,
      lucideCrown,
      lucideLock,
      lucideMoon,
      lucideNetwork,
      lucidePencil,
      lucidePlay,
      lucideUsers,
      lucidePower,
      lucideRefreshCw,
      lucideDatabaseBackup,
      lucideServer,
      lucideSettings,
      lucideShield,
      lucideSun,
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
          <span
            class="bg-muted text-muted-foreground rounded-full px-2 py-0.5 font-mono text-xs"
            data-testid="ns-badge"
            [attr.aria-label]="'clusterDetail.namespace' | transloco"
          >
            {{ d.namespace }}
          </span>
          <span class="text-muted-foreground font-mono text-xs tabular-nums">
            {{ d.ready_replicas }}/{{ d.replicas }}
          </span>
          @if (!d.hibernated) {
            <button
              hlmBtn
              variant="outline"
              size="sm"
              class="ml-auto"
              data-testid="open-hibernate"
              [disabled]="busy()"
              (click)="openHibernate(d)"
            >
              <ng-icon name="lucideMoon" size="14" />
              {{ 'clusterDetail.hibernate' | transloco }}
            </button>
          }
          <button
            hlmBtn
            variant="outline"
            size="sm"
            [class.ml-auto]="d.hibernated"
            data-testid="open-edit"
            [disabled]="busy() || d.hibernated"
            (click)="openEdit(d)"
          >
            <ng-icon name="lucidePencil" size="14" />
            {{ 'clusterDetail.edit' | transloco }}
          </button>
        </div>
        <p class="text-muted-foreground mb-4 flex items-center gap-1 text-sm">
          <span class="font-mono text-xs">{{ d.image }}</span>
          <button
            hlmBtn
            variant="ghost"
            size="sm"
            class="h-6 px-1.5"
            data-testid="open-image"
            [disabled]="busy() || d.hibernated"
            [attr.aria-label]="'image.edit' | transloco"
            (click)="openImage(d)"
          >
            <ng-icon name="lucidePencil" size="12" />
          </button>
          · {{ d.storage_size }} · {{ 'clusterDetail.primaryIs' | transloco: { pod: d.primary } }}
        </p>

        @if (d.hibernated) {
          <!-- Asleep on purpose: instances are gone but every volume is kept.
               Rendered instead of the instance table, whose probes would all
               read "unknown" and look like an outage. -->
          <div
            class="border-border bg-muted/40 mb-4 flex items-center gap-2 rounded-lg border p-3 text-sm"
            data-testid="hibernated-banner"
          >
            <ng-icon name="lucideMoon" size="18" class="text-muted-foreground" />
            <span>{{ 'clusterDetail.hibernatedBanner' | transloco: { count: d.replicas } }}</span>
            <button
              hlmBtn
              size="sm"
              class="ml-auto"
              data-testid="open-wake"
              [disabled]="busy()"
              (click)="wakeCluster()"
            >
              <ng-icon name="lucideSun" size="14" />
              {{ 'clusterDetail.wake' | transloco }}
            </button>
          </div>
        }

        @if (d.restore; as r) {
          <!-- Provenance matters: someone looking at this cluster months later
               needs to know its data is a copy, and of what. -->
          <p
            class="border-border bg-muted/40 text-muted-foreground mb-4 rounded-lg border px-3 py-2 text-xs leading-relaxed"
            data-testid="restored-from"
          >
            @if (r.target_time) {
              {{
                'restore.provenanceAtTime'
                  | transloco: { source: r.source_cluster, time: formatIsoTime(r.target_time) }
              }}
            } @else {
              {{
                'restore.provenance'
                  | transloco: { source: r.source_cluster, label: r.backup_label ?? '' }
              }}
            }
          </p>
        }

        @if (!d.hibernated) {
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
                      [class.bg-muted]="i.role === 'fenced'"
                      [class.text-muted-foreground]="i.role === 'fenced'"
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
                      @if (i.ssl) {
                        <ng-icon
                          name="lucideLock"
                          size="13"
                          class="text-success"
                          [attr.data-testid]="'ssl-' + i.pod"
                          [attr.aria-label]="'tls.instanceOn' | transloco"
                        />
                      }
                    </span>
                  </td>
                  <td hlmTd class="font-mono text-xs tabular-nums">{{ i.lsn ?? '—' }}</td>
                  <td hlmTd>
                    @if (i.pod !== d.primary && i.ready && i.role === 'standby') {
                      <button
                        hlmBtn
                        variant="ghost"
                        size="sm"
                        [disabled]="busy() || d.hibernated"
                        [attr.data-testid]="'switchover-' + i.pod"
                        (click)="openSwitchover(d, i.pod)"
                      >
                        <ng-icon name="lucideRefreshCw" size="14" />
                        {{ 'clusterDetail.makePrimary' | transloco }}
                      </button>
                    }
                    @if (i.role === 'fenced') {
                      <button
                        hlmBtn
                        variant="ghost"
                        size="sm"
                        [disabled]="busy()"
                        [attr.data-testid]="'unfence-' + i.pod"
                        (click)="unfence(i.pod)"
                      >
                        <ng-icon name="lucidePlay" size="14" />
                        {{ 'clusterDetail.unfence' | transloco }}
                      </button>
                    } @else if (i.pod !== d.primary) {
                      <button
                        hlmBtn
                        variant="ghost"
                        size="sm"
                        class="text-muted-foreground"
                        [disabled]="busy()"
                        [attr.data-testid]="'fence-' + i.pod"
                        (click)="openFence(i.pod)"
                      >
                        <ng-icon name="lucideBan" size="14" />
                        {{ 'clusterDetail.fence' | transloco }}
                      </button>
                    }
                  </td>
                </tr>
              }
            </tbody>
          </table>
        </div>
        }

        <!-- Placement and resources -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'workload.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-workload"
            [disabled]="busy() || d.hibernated"
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

        <!-- Backups -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'backups.title' | transloco }}</h2>
          <span class="flex items-center gap-2">
            @if (backups()?.enabled) {
              <button
                hlmBtn
                size="sm"
                data-testid="backup-now"
                [disabled]="busy() || backupRunning()"
                [hlmDropdownMenuTrigger]="backupKindMenu"
              >
                <ng-icon name="lucideDatabaseBackup" size="14" />
                {{ (backupRunning() ? 'backups.running' : 'backups.backupNow') | transloco }}
              </button>
              <ng-template #backupKindMenu>
                <hlm-dropdown-menu class="w-64">
                  @for (k of backupKinds; track k) {
                    <button
                      hlmDropdownMenuItem
                      [attr.data-testid]="'backup-kind-' + k"
                      (click)="backupNow(k)"
                    >
                      <span class="grid gap-0.5">
                        <span>{{ 'backups.kinds.' + k | transloco }}</span>
                        <span class="text-muted-foreground text-xs">
                          {{ 'backups.kindHelp.' + k | transloco }}
                        </span>
                      </span>
                    </button>
                  }
                </hlm-dropdown-menu>
              </ng-template>
            }
            <button
              hlmBtn
              variant="outline"
              size="sm"
              data-testid="open-backup-config"
              [disabled]="busy() || d.hibernated"
              (click)="openBackupConfig(d)"
            >
              {{ 'backups.configure' | transloco }}
            </button>
          </span>
        </div>

        @if (backups(); as b) {
          @if (!b.enabled) {
            <p class="text-muted-foreground mt-2 text-sm">{{ 'backups.disabled' | transloco }}</p>
          } @else {
            <p class="text-muted-foreground mt-2 text-sm">
              <span class="font-mono text-xs">{{ b.repository }}</span>
              · {{ 'backups.keeping' | transloco: { count: d.backup.retention_full } }}
              @if (b.schedule) {
                · {{ 'backups.scheduled' | transloco: { cron: b.schedule } }}
              } @else {
                · {{ 'backups.manualOnly' | transloco }}
              }
            </p>

            @if (b.runs.length) {
              <h3 class="text-muted-foreground mt-4 text-xs font-medium uppercase">
                {{ 'backups.runs' | transloco }}
              </h3>
              <div class="border-border mt-1 overflow-x-auto rounded-lg border">
                <table hlmTable class="w-full">
                  <thead hlmTHead>
                    <tr hlmTr>
                      <th hlmTh>{{ 'backups.state' | transloco }}</th>
                      <th hlmTh>{{ 'backups.kind' | transloco }}</th>
                      <th hlmTh>{{ 'backups.trigger' | transloco }}</th>
                      <th hlmTh>{{ 'backups.started' | transloco }}</th>
                      <th hlmTh>{{ 'backups.duration' | transloco }}</th>
                    </tr>
                  </thead>
                  <tbody hlmTBody>
                    @for (r of b.runs; track r.id) {
                      <tr hlmTr [attr.data-testid]="'run-' + r.id">
                        <td hlmTd>
                          <span class="inline-flex items-center gap-1.5 text-sm">
                            <span
                              class="h-2.5 w-2.5 rounded-full"
                              [class.bg-primary]="r.state === 'running'"
                              [class.animate-pulse]="r.state === 'running'"
                              [class.bg-success]="r.state === 'succeeded'"
                              [class.bg-destructive]="r.state === 'failed'"
                              [class.bg-warning]="r.state === 'unknown'"
                            ></span>
                            {{ 'backups.states.' + r.state | transloco }}
                          </span>
                        </td>
                        <td hlmTd class="text-sm">{{ 'backups.kinds.' + r.kind | transloco }}</td>
                        <td hlmTd class="text-muted-foreground text-sm">
                          {{ 'backups.triggers.' + r.trigger | transloco }}
                        </td>
                        <td hlmTd class="text-muted-foreground text-xs">
                          {{ formatTime(r.started) }}
                        </td>
                        <td hlmTd class="font-mono text-xs tabular-nums">
                          {{ formatDuration(r) }}
                        </td>
                      </tr>
                      @if (r.error) {
                        <tr hlmTr>
                          <td hlmTd colspan="5" class="text-destructive pt-0 text-xs">
                            {{ r.error }}
                          </td>
                        </tr>
                      }
                    }
                  </tbody>
                </table>
              </div>
              <h3 class="text-muted-foreground mt-4 text-xs font-medium uppercase">
                {{ 'backups.inRepository' | transloco }}
              </h3>
            }
            @if (b.backups.length) {
              <div class="border-border mt-2 overflow-x-auto rounded-lg border">
                <table hlmTable class="w-full">
                  <thead hlmTHead>
                    <tr hlmTr>
                      <th hlmTh>{{ 'backups.label' | transloco }}</th>
                      <th hlmTh>{{ 'backups.kind' | transloco }}</th>
                      <th hlmTh>{{ 'backups.size' | transloco }}</th>
                      <th hlmTh>{{ 'backups.finished' | transloco }}</th>
                      <th hlmTh class="text-right">{{ 'common.actions' | transloco }}</th>
                    </tr>
                  </thead>
                  <tbody hlmTBody>
                    @for (bk of b.backups; track bk.label) {
                      <tr hlmTr [attr.data-testid]="'backup-' + bk.label">
                        <td hlmTd class="font-mono text-xs">{{ bk.label }}</td>
                        <td hlmTd class="text-sm">{{ bk.kind }}</td>
                        <td hlmTd class="font-mono text-xs tabular-nums">
                          {{ formatSize(bk.size_bytes) }}
                        </td>
                        <td hlmTd class="text-muted-foreground text-xs">
                          {{ formatTime(bk.finished) }}
                        </td>
                        <td hlmTd class="text-right">
                          <button
                            hlmBtn
                            variant="outline"
                            size="sm"
                            [attr.data-testid]="'restore-' + bk.label"
                            [disabled]="busy() || d.hibernated"
                            (click)="openRestore(d, bk.label, bk.finished)"
                          >
                            {{ 'restore.action' | transloco }}
                          </button>
                        </td>
                      </tr>
                    }
                  </tbody>
                </table>
              </div>
            } @else {
              <p class="text-muted-foreground mt-2 text-sm">{{ 'backups.none' | transloco }}</p>
            }
          }
        }

        <!-- Secrets and initialisation -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'secrets.title' | transloco }}</h2>
          <div class="flex items-center gap-2">
            @if (!d.credentials_secret) {
              <button
                hlmBtn
                variant="outline"
                size="sm"
                data-testid="rotate-credentials"
                [disabled]="busy() || d.hibernated"
                (click)="openRotateCredentials()"
              >
                {{ 'secrets.rotate' | transloco }}
              </button>
            }
            <button
              hlmBtn
              variant="outline"
              size="sm"
              data-testid="open-env"
              [disabled]="busy() || d.hibernated"
              (click)="openEnv(d)"
            >
              {{ 'secrets.edit' | transloco }}
            </button>
          </div>
        </div>
        @if (secrets(); as s) {
          @if (!s.satisfied) {
            <p class="text-destructive mt-2 text-sm" role="alert" data-testid="secrets-unsatisfied">
              {{ 'secrets.unsatisfied' | transloco }}
            </p>
          }
          <div class="border-border mt-2 overflow-x-auto rounded-lg border">
            <table hlmTable class="w-full">
              <thead hlmTHead>
                <tr hlmTr>
                  <th hlmTh>{{ 'secrets.object' | transloco }}</th>
                  <th hlmTh>{{ 'secrets.purpose' | transloco }}</th>
                  <th hlmTh>{{ 'secrets.keys' | transloco }}</th>
                  <th hlmTh>{{ 'secrets.state' | transloco }}</th>
                </tr>
              </thead>
              <tbody hlmTBody>
                @for (r of s.resources; track r.name + r.kind) {
                  <tr hlmTr [attr.data-testid]="'secret-' + r.name">
                    <td hlmTd>
                      <span class="font-mono text-xs">{{ r.name }}</span>
                      <span class="text-muted-foreground ml-2 text-xs">{{ r.kind }}</span>
                    </td>
                    <td hlmTd class="text-sm">
                      {{ 'secrets.purposes.' + r.purpose | transloco }}
                    </td>
                    <td hlmTd class="text-muted-foreground font-mono text-xs">
                      {{ r.keys.length ? r.keys.join(', ') : '—' }}
                    </td>
                    <td hlmTd class="text-xs">
                      @if (r.managed) {
                        <span class="text-muted-foreground">
                          {{ 'secrets.states.managed' | transloco }}
                        </span>
                      } @else if (!r.exists) {
                        <span class="text-destructive font-medium">
                          {{ 'secrets.states.missing' | transloco }}
                        </span>
                      } @else if (r.missing_keys.length) {
                        <span class="text-destructive font-medium">
                          {{ 'secrets.states.missingKeys' | transloco }}
                          <span class="font-mono">{{ r.missing_keys.join(', ') }}</span>
                        </span>
                      } @else {
                        <span class="text-muted-foreground">
                          {{ 'secrets.states.present' | transloco }}
                        </span>
                      }
                    </td>
                  </tr>
                }
              </tbody>
            </table>
          </div>
        }

        <!-- Configuration -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'config.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-config"
            [disabled]="busy() || d.hibernated"
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
              [disabled]="busy() || d.hibernated"
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

        <!-- Synchronous replication -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'sync.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="toggle-sync"
            [disabled]="busy() || d.hibernated"
            (click)="openSync(d)"
          >
            <ng-icon name="lucideRefreshCw" size="14" />
            {{ (d.synchronous ? 'sync.disable' : 'sync.enable') | transloco }}
          </button>
        </div>

        @if (d.sync_degraded) {
          <div
            class="border-warning mt-2 flex items-center gap-2 rounded-lg border p-3 text-sm"
            data-testid="sync-degraded-banner"
          >
            <ng-icon name="lucideTriangleAlert" size="18" class="text-warning" />
            <span>{{ 'sync.degraded' | transloco }}</span>
          </div>
        }

        <p class="text-muted-foreground mt-2 text-sm" data-testid="sync-state">
          {{
            d.synchronous
              ? ('sync.on' | transloco: { count: syncStandbys(d) ?? '?' })
              : ('sync.off' | transloco)
          }}
        </p>

        <!-- Client access rules -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'hba.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-hba"
            [disabled]="busy() || d.hibernated"
            (click)="openHba(d)"
          >
            <ng-icon name="lucideShield" size="14" />
            {{ 'hba.edit' | transloco }}
          </button>
        </div>

        @if (d.pg_hba.length) {
          <div class="border-border mt-2 overflow-x-auto rounded-lg border">
            <table hlmTable class="w-full">
              <thead hlmTHead>
                <tr hlmTr>
                  <th hlmTh>{{ 'hba.conn' | transloco }}</th>
                  <th hlmTh>{{ 'hba.database' | transloco }}</th>
                  <th hlmTh>{{ 'hba.user' | transloco }}</th>
                  <th hlmTh>{{ 'hba.address' | transloco }}</th>
                  <th hlmTh>{{ 'hba.method' | transloco }}</th>
                </tr>
              </thead>
              <tbody hlmTBody>
                @for (r of d.pg_hba; track $index; let i = $index) {
                  <tr hlmTr [attr.data-testid]="'hba-view-row-' + i">
                    <td hlmTd class="font-mono text-xs">{{ r.conn }}</td>
                    <td hlmTd class="font-mono text-xs">{{ r.database }}</td>
                    <td hlmTd class="font-mono text-xs">{{ r.user }}</td>
                    <td hlmTd class="font-mono text-xs">{{ r.address }}</td>
                    <td hlmTd class="font-mono text-xs">{{ r.method }}</td>
                  </tr>
                }
              </tbody>
            </table>
          </div>
        } @else {
          <p class="text-muted-foreground mt-2 text-sm">{{ 'hba.noneSet' | transloco }}</p>
        }

        <!-- TLS -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'tls.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-tls"
            [disabled]="busy() || d.hibernated"
            (click)="openTls(d)"
          >
            <ng-icon name="lucideLock" size="14" />
            {{ 'tls.edit' | transloco }}
          </button>
        </div>
        <p class="text-muted-foreground mt-2 text-sm" data-testid="tls-state">
          {{
            d.tls.server_secret
              ? ('tls.on' | transloco: { secret: d.tls.server_secret })
              : ('tls.off' | transloco)
          }}
        </p>

        <!-- Pooler -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'pooler.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-pooler"
            [disabled]="busy() || d.hibernated"
            (click)="openPooler(d)"
          >
            <ng-icon name="lucideNetwork" size="14" />
            {{ 'pooler.edit' | transloco }}
          </button>
        </div>
        @if (d.pooler.spec.enabled) {
          <p class="text-muted-foreground mt-2 text-sm" data-testid="pooler-state">
            {{
              'pooler.on'
                | transloco
                  : {
                      ready: d.pooler.ready_replicas,
                      total: d.pooler.spec.replicas,
                      mode: d.pooler.spec.pool_mode,
                    }
            }}
          </p>
          <p class="text-muted-foreground mt-1 font-mono text-xs" data-testid="pooler-connect">
            {{ 'pooler.connectVia' | transloco: { service: d.pooler.service } }}
          </p>
        } @else {
          <p class="text-muted-foreground mt-2 text-sm" data-testid="pooler-state">
            {{ 'pooler.off' | transloco }}
          </p>
        }

        <!-- Roles & databases -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'provision.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="open-provision"
            [disabled]="busy() || d.hibernated"
            (click)="openProvision(d)"
          >
            <ng-icon name="lucideUsers" size="14" />
            {{ 'provision.edit' | transloco }}
          </button>
        </div>
        @if (d.roles.length || d.databases.length) {
          <p class="text-muted-foreground mt-2 text-sm" data-testid="provision-state">
            {{
              'provision.summary'
                | transloco: { roles: d.roles.length, databases: d.databases.length }
            }}
          </p>
          <div class="mt-1 grid gap-1">
            @for (r of d.roles; track r.name) {
              <p class="text-muted-foreground font-mono text-xs">
                {{ r.name }}{{ r.login === false ? ' · nologin' : '' }}
                {{ r.in_roles?.length ? '→ ' + r.in_roles!.join(', ') : '' }}
              </p>
            }
            @for (db of d.databases; track db.name) {
              <p class="text-muted-foreground font-mono text-xs">
                {{ db.name }} ({{ db.owner }})
              </p>
            }
          </div>
        } @else {
          <p class="text-muted-foreground mt-2 text-sm" data-testid="provision-state">
            {{ 'provision.none' | transloco }}
          </p>
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
  private readonly router = inject(Router);

  readonly ns = input.required<string>();
  readonly name = input.required<string>();

  protected readonly detail = signal<ClusterDetail | null>(null);
  protected readonly backups = signal<BackupList | null>(null);
  protected readonly secrets = signal<ResourceList | null>(null);
  protected readonly error = signal<string | null>(null);

  async openEnv(d: ClusterDetail): Promise<void> {
    const result = await firstValueFrom(
      this.dialog.open<EnvDialogResult | undefined>(ClusterEnvDialogComponent, {
        context: {
          credentialsSecret: d.credentials_secret,
          env: d.env ?? [],
          init: d.init ?? {},
          existing: true,
          generatedName: `${d.name}-credentials`,
        },
        contentClass: EDITOR_DIALOG_CLASS,
      }).closed$,
    );
    if (!result) {
      return;
    }
    // credentials_secret is fixed once a cluster exists, so it is not sent.
    await this.run(() =>
      this.api.invoke(patchCluster, {
        ns: this.ns(),
        name: this.name(),
        body: { env: result.env, init: result.init },
      }),
    );
  }

  protected formatSize(bytes: number): string {
    if (!bytes) {
      return '—';
    }
    const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024;
      unit++;
    }
    return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
  }

  protected formatTime(unixSeconds: number): string {
    if (!unixSeconds) {
      return '—';
    }
    return new Date(unixSeconds * 1000).toLocaleString();
  }

  protected formatIsoTime(iso: string): string {
    const at = new Date(iso);
    return isNaN(at.getTime()) ? iso : at.toLocaleString();
  }

  /**
   * Restore into a new cluster and go to it. Navigating is the point: the
   * restore happens on the *new* cluster, so leaving the user here watching an
   * unchanged page would look like nothing happened.
   */
  async openRestore(d: ClusterDetail, label?: string, finished?: number): Promise<void> {
    const req = await firstValueFrom(
      this.dialog.open<RestoreRequest | undefined>(ClusterRestoreDialogComponent, {
        context: { source: d.name, label, finished },
        contentClass: FORM_DIALOG_CLASS,
      }).closed$,
    );
    if (!req) {
      return;
    }
    const ok = await this.run(() =>
      this.api.invoke(restoreCluster, { ns: this.ns(), name: this.name(), body: req }),
    );
    if (ok) {
      await this.router.navigate(['/clusters', req.target_cluster]);
    }
  }

  async openBackupConfig(d: ClusterDetail): Promise<void> {
    const backup = await firstValueFrom(
      this.dialog.open<BackupSpec | undefined>(ClusterBackupDialogComponent, {
        context: { backup: d.backup },
        contentClass: FORM_DIALOG_CLASS,
      }).closed$,
    );
    if (!backup) {
      return;
    }
    await this.run(() => this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { backup } }));
  }

  protected readonly backupKinds = ['full', 'diff', 'incr'] as const;

  /** A backup is in flight; the button waits rather than queueing another. */
  protected backupRunning(): boolean {
    return (this.backups()?.runs ?? []).some((r) => r.state === 'running');
  }

  protected formatDuration(run: BackupRun): string {
    const end = run.finished ?? Math.floor(Date.now() / 1000);
    const seconds = Math.max(0, end - run.started);
    if (seconds < 60) {
      return `${seconds}s`;
    }
    const minutes = Math.floor(seconds / 60);
    return minutes < 60 ? `${minutes}m ${seconds % 60}s` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
  }

  async backupNow(kind: 'full' | 'diff' | 'incr'): Promise<void> {
    await this.run(() => this.api.invoke(createBackup, { ns: this.ns(), name: this.name(), body: { kind } }));
  }
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
    await this.run(() => this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { workload } }));
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
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { parameters } }),
    );
  }

  async openTls(d: ClusterDetail): Promise<void> {
    const result = await firstValueFrom(
      this.dialog.open<TlsSpec | undefined>(ClusterTlsDialogComponent, {
        context: { tls: d.tls ?? {} },
        contentClass: FORM_DIALOG_CLASS,
      }).closed$,
    );
    if (!result) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { tls: result } }),
    );
  }

  async openPooler(d: ClusterDetail): Promise<void> {
    const result = await firstValueFrom(
      this.dialog.open<PoolerSpec | undefined>(ClusterPoolerDialogComponent, {
        context: { pooler: d.pooler?.spec ?? {} },
        contentClass: FORM_DIALOG_CLASS,
      }).closed$,
    );
    if (!result) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { pooler: result } }),
    );
  }

  async openProvision(d: ClusterDetail): Promise<void> {
    const result = await firstValueFrom(
      this.dialog.open<ProvisionResult | undefined>(ClusterProvisionDialogComponent, {
        context: { roles: d.roles ?? [], databases: d.databases ?? [] },
        contentClass: EDITOR_DIALOG_CLASS,
      }).closed$,
    );
    if (!result) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, {
        ns: this.ns(),
        name: this.name(),
        body: { roles: result.roles, databases: result.databases },
      }),
    );
  }

  async openHba(d: ClusterDetail): Promise<void> {
    const rules = await firstValueFrom(
      this.dialog.open<HbaRule[] | undefined>(ClusterHbaDialogComponent, {
        context: { rules: d.pg_hba ?? [] },
        contentClass: EDITOR_DIALOG_CLASS,
      }).closed$,
    );
    if (!rules) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { pg_hba: rules } }),
    );
  }

  /** The primary's connected-sync-standby count, if it is reporting one. */
  protected syncStandbys(d: ClusterDetail): number | null {
    const p = d.instances?.find((i) => i.pod === d.primary);
    return p?.sync_standby_count ?? null;
  }

  async openSync(d: ClusterDetail): Promise<void> {
    const enable = !d.synchronous;
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(SyncDialogComponent, {
        context: { ns: this.ns(), name: this.name(), enable },
      }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { synchronous: enable } }),
    );
  }

  async openRestart(): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(RestartDialogComponent, { context: { ns: this.ns(), name: this.name() } })
        .closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() => this.api.invoke(restartCluster, { ns: this.ns(), name: this.name() }));
  }

  async openRotateCredentials(): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(RotateCredentialsDialogComponent, {
        context: { ns: this.ns(), name: this.name() },
      }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() => this.api.invoke(rotateCredentials, { ns: this.ns(), name: this.name() }));
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
    await this.run(() => this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: change }));
  }

  async openImage(d: ClusterDetail): Promise<void> {
    const next = await firstValueFrom(
      this.dialog.open<string | undefined>(ImageDialogComponent, {
        context: { ns: this.ns(), name: this.name(), image: d.image },
        contentClass: FORM_DIALOG_CLASS,
      }).closed$,
    );
    if (!next) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { image: next } }),
    );
  }

  async openSwitchover(d: ClusterDetail, pod: string): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(SwitchoverDialogComponent, { context: { pod } }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() =>
      this.api.invoke(switchover, { ns: this.ns(), name: this.name(), body: { target: pod } }),
    );
  }

  async openHibernate(d: ClusterDetail): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(HibernateDialogComponent, { context: { name: d.name } }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() => this.api.invoke(hibernate, { ns: this.ns(), name: this.name() }));
  }

  /** Waking is safe (it only restores instances) — no confirmation. */
  async wakeCluster(): Promise<void> {
    await this.run(() => this.api.invoke(wake, { ns: this.ns(), name: this.name() }));
  }

  async openFence(pod: string): Promise<void> {
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(FenceDialogComponent, { context: { pod } }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() => this.api.invoke(fenceInstance, { ns: this.ns(), name: this.name(), pod }));
  }

  async unfence(pod: string): Promise<void> {
    await this.run(() => this.api.invoke(unfenceInstance, { ns: this.ns(), name: this.name(), pod }));
  }

  /** Returns whether the operation succeeded, so callers can act on it. */
  private async run(op: () => Promise<unknown>): Promise<boolean> {
    this.busy.set(true);
    try {
      await op();
      this.error.set(null);
      await this.refresh();
      return true;
    } catch (err) {
      const e = err as { error?: { detail?: string; code?: string }; message?: string };
      this.error.set(e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed');
      return false;
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
      this.detail.set(await this.api.invoke(getCluster, { ns: this.ns(), name: this.name() }));
      this.error.set(null);
    } catch (err) {
      const e = err as { error?: { detail?: string; code?: string }; message?: string };
      this.error.set(e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed');
      return;
    }
    // Reading the repository can be slow or unavailable; it must never take
    // the rest of the page down with it.
    try {
      this.backups.set(await this.api.invoke(listBackups, { ns: this.ns(), name: this.name() }));
    } catch {
      this.backups.set(null);
    }
    // Same reasoning: this reads other objects in the namespace, and a
    // permission gap there must not blank the whole page.
    try {
      this.secrets.set(await this.api.invoke(listRequiredSecrets, { ns: this.ns(), name: this.name() }));
    } catch {
      this.secrets.set(null);
    }
  }
}

function summarizeMap(map: Record<string, string> | undefined): string {
  const entries = Object.entries(map ?? {});
  return entries.map(([k, v]) => `${k}=${v}`).join(' · ');
}
