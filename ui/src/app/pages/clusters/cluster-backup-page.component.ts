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
import { problemDetail } from '../../core/problem';
import { BreadcrumbsComponent } from '../../shell/breadcrumbs.component';

/** Configure the backup repository: where pgBackRest stores this cluster's
 * backups and WAL, and when it takes them. */
@Component({
  selector: 'pg-cluster-backup-page',
  imports: [FormsModule, TranslocoModule, HlmButton, HlmInput, HlmLabel, BreadcrumbsComponent],
  template: `
    <div class="mx-auto max-w-2xl pb-24">
      <pg-breadcrumbs [crumbs]="crumbs()" />
      <h1 class="mb-1 text-lg font-semibold">{{ 'backups.configureTitle' | transloco }}</h1>
      <p class="text-muted-foreground mb-6 text-sm">{{ 'backups.intro' | transloco }}</p>

      @if (error(); as err) {
        <p class="text-destructive mb-4 text-sm" role="alert">{{ err }}</p>
      }

      @if (loading()) {
        <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
      } @else {
        <div class="grid gap-6">
          <label class="flex cursor-pointer items-start gap-3">
            <input
              type="checkbox"
              class="mt-1 h-4 w-4 cursor-pointer"
              data-testid="backup-enabled"
              [ngModel]="enabled()"
              (ngModelChange)="enabled.set($event)"
            />
            <span class="grid gap-1">
              <span class="text-sm font-medium">{{ 'backups.enable' | transloco }}</span>
              <span class="text-muted-foreground text-xs leading-relaxed">
                {{ 'backups.enableHelp' | transloco }}
              </span>
            </span>
          </label>

          @if (enabled()) {
            <div class="border-border grid gap-4 border-t pt-6">
              <div class="grid gap-2">
                <label hlmLabel for="bk-endpoint">{{ 'backups.endpoint' | transloco }}</label>
                <input
                  hlmInput
                  id="bk-endpoint"
                  data-testid="backup-endpoint"
                  placeholder="https://s3.eu-west-1.amazonaws.com"
                  [ngModel]="endpoint()"
                  (ngModelChange)="endpoint.set($event)"
                />
                <p
                  class="text-xs leading-relaxed"
                  [class.text-destructive]="endpoint() && !endpointValid()"
                  [class.text-muted-foreground]="!endpoint() || endpointValid()"
                >
                  {{ 'backups.endpointHelp' | transloco }}
                </p>
              </div>

              <div class="grid gap-4 sm:grid-cols-2">
                <div class="grid gap-2">
                  <label hlmLabel for="bk-bucket">{{ 'backups.bucket' | transloco }}</label>
                  <input
                    hlmInput
                    id="bk-bucket"
                    data-testid="backup-bucket"
                    [ngModel]="bucket()"
                    (ngModelChange)="bucket.set($event)"
                  />
                </div>
                <div class="grid gap-2">
                  <label hlmLabel for="bk-region">{{ 'backups.region' | transloco }}</label>
                  <input
                    hlmInput
                    id="bk-region"
                    [ngModel]="region()"
                    (ngModelChange)="region.set($event)"
                  />
                </div>
              </div>

              <div class="grid gap-2">
                <label hlmLabel for="bk-secret">{{ 'backups.secret' | transloco }}</label>
                <input
                  hlmInput
                  id="bk-secret"
                  data-testid="backup-secret"
                  placeholder="backup-credentials"
                  [ngModel]="secret()"
                  (ngModelChange)="secret.set($event)"
                />
                <p class="text-muted-foreground text-xs leading-relaxed">
                  {{ 'backups.secretHelp' | transloco }}
                </p>
              </div>

              <div class="grid gap-4 sm:grid-cols-2">
                <div class="grid gap-2">
                  <label hlmLabel for="bk-prefix">{{ 'backups.prefix' | transloco }}</label>
                  <input
                    hlmInput
                    id="bk-prefix"
                    [ngModel]="prefix()"
                    (ngModelChange)="prefix.set($event)"
                  />
                </div>
                <div class="grid gap-2">
                  <label hlmLabel for="bk-retention">{{ 'backups.retention' | transloco }}</label>
                  <input
                    hlmInput
                    id="bk-retention"
                    type="number"
                    min="1"
                    max="52"
                    [ngModel]="retention()"
                    (ngModelChange)="retention.set(+$event)"
                  />
                </div>
              </div>

              <div class="border-border grid gap-4 border-t pt-6">
                <div class="grid gap-2">
                  <label hlmLabel for="bk-schedule">{{ 'backups.schedule' | transloco }}</label>
                  <input
                    hlmInput
                    id="bk-schedule"
                    data-testid="backup-schedule"
                    placeholder="0 3 * * *"
                    class="font-mono"
                    [ngModel]="schedule()"
                    (ngModelChange)="schedule.set($event)"
                  />
                  <p class="text-muted-foreground text-xs leading-relaxed">
                    {{ 'backups.scheduleHelp' | transloco }}
                  </p>
                </div>

                @if (schedule().trim()) {
                  <div class="grid gap-2">
                    <span hlmLabel>{{ 'backups.scheduleKind' | transloco }}</span>
                    <div class="bg-muted grid grid-cols-3 gap-1 rounded-lg p-1" role="tablist">
                      @for (k of kinds; track k) {
                        <button
                          type="button"
                          class="cursor-pointer rounded-md px-2 py-1.5 text-sm font-medium transition-colors"
                          [class.bg-background]="scheduleKind() === k"
                          [class.shadow-sm]="scheduleKind() === k"
                          [class.text-muted-foreground]="scheduleKind() !== k"
                          role="tab"
                          [attr.aria-selected]="scheduleKind() === k"
                          [attr.data-testid]="'schedule-kind-' + k"
                          (click)="scheduleKind.set(k)"
                        >
                          {{ 'backups.kinds.' + k | transloco }}
                        </button>
                      }
                    </div>
                    <p class="text-muted-foreground text-xs leading-relaxed">
                      {{ 'backups.kindHelp.' + scheduleKind() | transloco }}
                    </p>
                  </div>
                }
              </div>

              <label class="flex cursor-pointer items-start gap-3">
                <input
                  type="checkbox"
                  class="mt-1 h-4 w-4 cursor-pointer"
                  data-testid="backup-verify-tls"
                  [ngModel]="verifyTls()"
                  (ngModelChange)="verifyTls.set($event)"
                />
                <span class="grid gap-1">
                  <span class="text-sm font-medium">{{ 'backups.verifyTls' | transloco }}</span>
                  <span class="text-muted-foreground text-xs leading-relaxed">
                    {{ 'backups.verifyTlsHelp' | transloco }}
                  </span>
                </span>
              </label>

              <p class="text-muted-foreground text-xs leading-relaxed">
                {{ 'backups.restartNote' | transloco }}
              </p>
            </div>
          }
        </div>
      }
    </div>

    <!-- Sticky action bar: on a long form the primary action must stay
         reachable without scrolling back down. -->
    <div class="bg-background/95 border-border fixed inset-x-0 bottom-0 border-t backdrop-blur">
      <div class="mx-auto flex max-w-2xl items-center gap-3 px-4 py-3">
        <div class="ml-auto flex gap-2">
          <button hlmBtn variant="ghost" (click)="cancel()">
            {{ 'common.cancel' | transloco }}
          </button>
          <button
            hlmBtn
            data-testid="backup-submit"
            [disabled]="loading() || busy() || !valid()"
            (click)="submit()"
          >
            {{ 'backups.save' | transloco }}
          </button>
        </div>
      </div>
    </div>
  `,
})
export class ClusterBackupPageComponent {
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
    { label: 'Backups' },
  ]);

  protected readonly enabled = signal(false);
  protected readonly endpoint = signal('');
  protected readonly bucket = signal('');
  protected readonly region = signal('us-east-1');
  protected readonly secret = signal('');
  protected readonly prefix = signal('');
  protected readonly retention = signal(2);
  protected readonly verifyTls = signal(true);
  protected readonly schedule = signal('');
  protected readonly kinds = ['full', 'diff', 'incr'] as const;
  protected readonly scheduleKind = signal<'full' | 'diff' | 'incr'>('full');

  /** pgBackRest requires TLS, so only https endpoints are accepted. */
  protected readonly endpointValid = computed(() => /^https:\/\/.+/.test(this.endpoint().trim()));
  protected readonly valid = computed(() => {
    if (!this.enabled()) {
      return true;
    }
    return (
      this.endpointValid() &&
      this.bucket().trim().length > 0 &&
      this.secret().trim().length > 0 &&
      this.retention() >= 1 &&
      this.retention() <= 52
    );
  });

  constructor() {
    queueMicrotask(() => void this.load());
  }

  private async load(): Promise<void> {
    try {
      const d = await this.api.invoke(getCluster, { ns: this.ns(), name: this.name() });
      const backup = d.backup;
      this.enabled.set(backup.enabled ?? false);
      this.endpoint.set(backup.s3_endpoint ?? '');
      this.bucket.set(backup.s3_bucket ?? '');
      this.region.set(backup.s3_region ?? 'us-east-1');
      this.secret.set(backup.s3_credentials_secret ?? '');
      this.prefix.set(backup.path_prefix ?? '');
      this.retention.set(backup.retention_full ?? 2);
      this.verifyTls.set(backup.verify_tls ?? true);
      this.schedule.set(backup.schedule ?? '');
      this.scheduleKind.set(backup.schedule_kind ?? 'full');
      this.error.set(null);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.loading.set(false);
    }
  }

  protected cancel(): void {
    void this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
  }

  protected async submit(): Promise<void> {
    const trimmed = (s: string) => (s.trim() ? s.trim() : undefined);
    const backup = {
      enabled: this.enabled(),
      s3_endpoint: trimmed(this.endpoint()),
      s3_bucket: trimmed(this.bucket()),
      s3_region: this.region().trim() || 'us-east-1',
      s3_credentials_secret: trimmed(this.secret()),
      path_prefix: trimmed(this.prefix()),
      retention_full: this.retention(),
      verify_tls: this.verifyTls(),
      schedule: trimmed(this.schedule()),
      schedule_kind: this.scheduleKind(),
    };

    this.busy.set(true);
    this.error.set(null);
    try {
      await this.api.invoke(patchCluster, { ns: this.ns(), name: this.name(), body: { backup } });
      await this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.busy.set(false);
    }
  }
}
