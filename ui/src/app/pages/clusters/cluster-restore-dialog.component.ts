import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { RestoreRequest } from '../../api/models/restore-request';

export interface RestoreDialogContext {
  /** Cluster being restored from. */
  source: string;
  /** Backup the restore starts from; undefined means the most recent. */
  label?: string;
  /** When that backup finished, as a hint for the point-in-time field. */
  finished?: number;
}

/**
 * Unix seconds -> the local-time string a datetime-local input expects.
 * Deliberately not toISOString(): that is UTC, and the input would then show a
 * time the user never chose.
 */
function toDateTimeLocal(epochSeconds: number): string {
  const at = new Date(epochSeconds * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return (
    `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}` +
    `T${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}`
  );
}

/** Restore a backup into a new cluster. Resolves to the request, or undefined. */
@Component({
  selector: 'pg-cluster-restore-dialog',
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
      <h2 hlmDialogTitle>{{ 'restore.title' | transloco }}</h2>
      <p class="text-muted-foreground mt-1 text-sm leading-relaxed">
        {{ 'restore.subtitle' | transloco: { source: ctx.source } }}
      </p>
    </div>

    <div class="pg-scroll grid max-h-[min(65vh,32rem)] gap-6 overflow-y-auto py-1 pr-2">
      <div class="grid gap-2">
        <label hlmLabel for="rs-name">{{ 'restore.targetName' | transloco }}</label>
        <input
          hlmInput
          id="rs-name"
          data-testid="restore-target"
          autocomplete="off"
          [ngModel]="target()"
          (ngModelChange)="target.set($event)"
        />
        <!-- Always rendered, only recoloured: a message that appears on error
             would resize the dialog under the pointer. -->
        <p
          class="text-xs leading-relaxed"
          [class.text-destructive]="target() && !nameValid()"
          [class.text-muted-foreground]="!target() || nameValid()"
        >
          {{ 'restore.targetHelp' | transloco }}
        </p>
      </div>

      <div class="border-border grid gap-4 border-t pt-6">
        <span hlmLabel>{{ 'restore.point' | transloco }}</span>
        <div class="bg-muted grid grid-cols-3 gap-1 rounded-lg p-1" role="tablist">
          @for (m of modes; track m) {
            <button
              type="button"
              class="cursor-pointer rounded-md px-2 py-1.5 text-sm font-medium transition-colors"
              [class.bg-background]="mode() === m"
              [class.shadow-sm]="mode() === m"
              [class.text-muted-foreground]="mode() !== m"
              role="tab"
              [attr.aria-selected]="mode() === m"
              [attr.data-testid]="'restore-mode-' + m"
              (click)="mode.set(m)"
            >
              {{ 'restore.modes.' + m | transloco }}
            </button>
          }
        </div>

        @if (mode() === 'backup') {
          <p class="text-muted-foreground text-xs leading-relaxed">
            {{ 'restore.fromBackup' | transloco: { label: ctx.label ?? '' } }}
          </p>
        } @else if (mode() === 'latest') {
          <p class="text-muted-foreground text-xs leading-relaxed">
            {{ 'restore.fromLatest' | transloco }}
          </p>
        } @else {
          <div class="grid gap-2">
            <label hlmLabel for="rs-time">{{ 'restore.targetTime' | transloco }}</label>
            <input
              hlmInput
              id="rs-time"
              type="datetime-local"
              step="1"
              data-testid="restore-time"
              [ngModel]="targetTime()"
              (ngModelChange)="targetTime.set($event)"
            />
            <p
              class="text-xs leading-relaxed"
              [class.text-destructive]="mode() === 'time' && !targetTime()"
              [class.text-muted-foreground]="mode() !== 'time' || !!targetTime()"
            >
              {{ 'restore.targetTimeHelp' | transloco }}
            </p>
          </div>
        }
      </div>

      <p class="border-border text-muted-foreground border-t pt-6 text-xs leading-relaxed">
        {{ 'restore.note' | transloco }}
      </p>
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="restore-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'restore.submit' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterRestoreDialogComponent {
  protected readonly ref = inject<BrnDialogRef<RestoreRequest | undefined>>(BrnDialogRef);
  protected readonly ctx = injectBrnDialogContext<RestoreDialogContext>();

  protected readonly modes = ['backup', 'latest', 'time'] as const;
  /** Opened from a backup row, so that backup is the default selection. */
  protected readonly mode = signal<'backup' | 'latest' | 'time'>(
    this.ctx.label ? 'backup' : 'latest',
  );
  protected readonly target = signal(`${this.ctx.source}-restored`);
  /** Anchored on the chosen backup: a point in time is only reachable at or
   *  after the backup it is recovered from, so an empty field is a worse
   *  starting point than the one time we know is valid. */
  protected readonly targetTime = signal(
    this.ctx.finished ? toDateTimeLocal(this.ctx.finished) : '',
  );

  /** Same RFC1123 rule the server enforces, so the error arrives before the request. */
  protected readonly nameValid = computed(() => {
    const name = this.target().trim();
    return (
      /^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(name) && name.length <= 40 && name !== this.ctx.source
    );
  });

  protected readonly valid = computed(
    () => this.nameValid() && (this.mode() === 'backup' || !!this.targetTime()),
  );

  protected submit(): void {
    this.ref.close({
      target_cluster: this.target().trim(),
      // Only pinned in 'backup' mode. For a point in time, pgBackRest picks the
      // backup that can actually reach the target itself; pinning one here is a
      // way to ask for a target the chosen backup cannot reach.
      backup_label: this.mode() === 'backup' ? this.ctx.label : undefined,
      target_time: this.mode() === 'time' ? new Date(this.targetTime()).toISOString() : undefined,
    });
  }
}
