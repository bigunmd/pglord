import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { BackupSpec } from '../../api/models/backup-spec';

/** Configure the backup repository. Resolves to the block, or undefined. */
@Component({
  selector: 'pg-cluster-backup-dialog',
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
      <h2 hlmDialogTitle>{{ 'backups.configureTitle' | transloco }}</h2>
    </div>

    <div class="pg-scroll grid max-h-[min(65vh,32rem)] gap-6 overflow-y-auto py-1 pr-2">
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

          <p class="text-muted-foreground text-xs leading-relaxed">
            {{ 'backups.restartNote' | transloco }}
          </p>
        </div>
      }
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="backup-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'backups.save' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterBackupDialogComponent {
  protected readonly ref = inject<BrnDialogRef<BackupSpec | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ backup: BackupSpec }>();

  protected readonly enabled = signal(this.ctx.backup.enabled ?? false);
  protected readonly endpoint = signal(this.ctx.backup.s3_endpoint ?? '');
  protected readonly bucket = signal(this.ctx.backup.s3_bucket ?? '');
  protected readonly region = signal(this.ctx.backup.s3_region ?? 'us-east-1');
  protected readonly secret = signal(this.ctx.backup.s3_credentials_secret ?? '');
  protected readonly prefix = signal(this.ctx.backup.path_prefix ?? '');
  protected readonly retention = signal(this.ctx.backup.retention_full ?? 2);

  protected readonly endpointValid = computed(() => /^https?:\/\/.+/.test(this.endpoint().trim()));
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

  protected submit(): void {
    const trimmed = (s: string) => (s.trim() ? s.trim() : undefined);
    this.ref.close({
      enabled: this.enabled(),
      s3_endpoint: trimmed(this.endpoint()),
      s3_bucket: trimmed(this.bucket()),
      s3_region: this.region().trim() || 'us-east-1',
      s3_credentials_secret: trimmed(this.secret()),
      path_prefix: trimmed(this.prefix()),
      retention_full: this.retention(),
    });
  }
}
