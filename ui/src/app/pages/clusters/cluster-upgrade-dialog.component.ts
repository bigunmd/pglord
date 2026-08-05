import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { TranslocoModule } from '@jsverse/transloco';

import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogBody, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

/** Irreversible steps of an upgrade, each with its own confirmation copy. */
export type UpgradeConfirmKind = 'cutover' | 'rollback' | 'finalize' | 'abort';

/** The two ways a major upgrade can run; they differ in what they cost, which
 * is the whole basis for choosing. */
export type UpgradeMode = 'blue_green' | 'in_place';

/** What starting an upgrade resolves to, or undefined on cancel. */
export interface UpgradeStartResult {
  image: string;
  mode: UpgradeMode;
}

/** Start a major version upgrade: pick a mode and a target image. */
@Component({
  selector: 'pg-upgrade-start-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    HlmButton,
    HlmDialogBody,
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
    <div hlmDialogBody>
  <div class="grid gap-3 py-1">
        <fieldset class="grid gap-2">
          <legend hlmLabel class="mb-1">{{ 'upgrade.mode' | transloco }}</legend>
          @for (m of modes; track m.value) {
            <label
              class="border-border hover:bg-accent/40 flex cursor-pointer gap-2 rounded-lg border p-2"
              [attr.data-testid]="'upgrade-mode-' + m.value"
            >
              <input
                type="radio"
                name="pg-upgrade-mode"
                class="mt-1"
                [value]="m.value"
                [ngModel]="mode()"
                (ngModelChange)="mode.set($event)"
              />
              <span class="grid gap-0.5">
                <span class="text-sm font-medium">{{ m.label | transloco }}</span>
                <span class="text-muted-foreground text-xs leading-relaxed">
                  {{ m.hint | transloco }}
                </span>
              </span>
            </label>
          }
        </fieldset>

        <div class="grid gap-2">
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
            {{
              (mode() === 'in_place' ? 'upgrade.inPlaceNeedsBackup' : 'upgrade.startWarning')
                | transloco
            }}
          </p>
        </div>
      </div>
    </div>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button
        hlmBtn
        data-testid="confirm-upgrade-start"
        [disabled]="!canStart()"
        (click)="ref.close({ image: image().trim(), mode: mode() })"
      >
        {{ 'upgrade.start' | transloco }}
      </button>
    </div>
  `,
})
export class UpgradeStartDialogComponent {
  protected readonly ref = inject<BrnDialogRef<UpgradeStartResult | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string }>();
  protected readonly name = this.ctx.name;
  protected readonly image = signal('');
  /** Blue-green by default: it is the one that cannot lose data. */
  protected readonly mode = signal<UpgradeMode>('blue_green');
  protected readonly canStart = computed(() => this.image().trim().length > 0);
  protected readonly modes = [
    {
      value: 'blue_green' as const,
      label: 'upgrade.modeBlueGreen',
      hint: 'upgrade.modeBlueGreenHint',
    },
    { value: 'in_place' as const, label: 'upgrade.modeInPlace', hint: 'upgrade.modeInPlaceHint' },
  ];
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
    <div hlmDialogBody>
  <p class="text-muted-foreground py-1 text-sm leading-relaxed">
        {{ warningKey | transloco }}
      </p>
    </div>
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
  private readonly ctx = injectBrnDialogContext<{
    name: string;
    kind: UpgradeConfirmKind;
    inPlace?: boolean;
  }>();
  protected readonly name = this.ctx.name;
  protected readonly kind = this.ctx.kind;
  /** In-place aborts undo far less than blue-green ones, so they say so. */
  private readonly copyKind =
    this.ctx.kind === 'abort' && this.ctx.inPlace ? 'abortInPlace' : this.ctx.kind;
  protected readonly titleKey = `upgrade.${this.copyKind}Title`;
  protected readonly warningKey = `upgrade.${this.copyKind}Warning`;
  protected readonly actionKey = `upgrade.${this.ctx.kind}`;
  /** Everything but the cutover throws data or instances away. */
  protected readonly destructive = this.ctx.kind !== 'cutover';
}
