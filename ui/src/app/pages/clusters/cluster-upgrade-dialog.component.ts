import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { TranslocoModule } from '@jsverse/transloco';

import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

/** Irreversible steps of an upgrade, each with its own confirmation copy. */
export type UpgradeConfirmKind = 'cutover' | 'rollback' | 'finalize' | 'abort';

/** Start a major version upgrade. Resolves to the target image, or undefined
 * on cancel. */
@Component({
  selector: 'pg-upgrade-start-dialog',
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
      <h2 hlmDialogTitle>{{ 'upgrade.startTitle' | transloco: { name: name } }}</h2>
    </div>
    <div class="grid gap-2 py-1">
      <label hlmLabel for="pg-upgrade-image">{{ 'upgrade.targetImage' | transloco }}</label>
      <input
        hlmInput
        id="pg-upgrade-image"
        class="font-mono"
        data-testid="upgrade-image-input"
        [ngModel]="image()"
        (ngModelChange)="image.set($event)"
      />
      <p class="text-muted-foreground text-xs leading-relaxed">
        {{ 'upgrade.startWarning' | transloco }}
      </p>
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button
        hlmBtn
        data-testid="confirm-upgrade-start"
        [disabled]="!canStart()"
        (click)="ref.close(image().trim())"
      >
        {{ 'upgrade.start' | transloco }}
      </button>
    </div>
  `,
})
export class UpgradeStartDialogComponent {
  protected readonly ref = inject<BrnDialogRef<string | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string }>();
  protected readonly name = this.ctx.name;
  protected readonly image = signal('');
  protected readonly canStart = computed(() => this.image().trim().length > 0);
}

/** Confirm one of the upgrade's irreversible steps. Resolves to true when
 * confirmed. */
@Component({
  selector: 'pg-upgrade-confirm-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ titleKey | transloco: { name: name } }}</h2>
    </div>
    <p class="text-muted-foreground py-1 text-sm leading-relaxed">
      {{ warningKey | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button
        hlmBtn
        [variant]="destructive ? 'destructive' : 'default'"
        [attr.data-testid]="'confirm-upgrade-' + kind"
        (click)="ref.close(true)"
      >
        {{ actionKey | transloco }}
      </button>
    </div>
  `,
})
export class UpgradeConfirmDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string; kind: UpgradeConfirmKind }>();
  protected readonly name = this.ctx.name;
  protected readonly kind = this.ctx.kind;
  protected readonly titleKey = `upgrade.${this.ctx.kind}Title`;
  protected readonly warningKey = `upgrade.${this.ctx.kind}Warning`;
  protected readonly actionKey = `upgrade.${this.ctx.kind}`;
  /** Everything but the cutover throws data or instances away. */
  protected readonly destructive = this.ctx.kind !== 'cutover';
}
