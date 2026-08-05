import type { BooleanInput } from '@angular/cdk/coercion';
import type { ComponentType } from '@angular/cdk/portal';
import { NgComponentOutlet } from '@angular/common';
import {
  booleanAttribute,
  ChangeDetectionStrategy,
  Component,
  computed,
  inject,
  input,
  ViewEncapsulation,
} from '@angular/core';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideX } from '@ng-icons/lucide';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';

import { classes } from '@spartan-ng/helm/utils';
import { HlmDialogClose } from './hlm-dialog-close';

type HlmDialogContentContext = {
  $component?: ComponentType<unknown>;
  $dynamicComponentClass?: string;
  $showCloseButton?: boolean;
};

@Component({
  selector: 'hlm-dialog-content',
  imports: [NgComponentOutlet, HlmButton, HlmDialogClose, NgIcon],
  providers: [provideIcons({ lucideX })],
  changeDetection: ChangeDetectionStrategy.OnPush,
  // The dialog's real content is a dynamically created component, so its host
  // element sits between this box and the header/body/footer. Left unstyled it
  // is a plain block that swallows the layout — which is why a grid here never
  // laid out the three rows. Encapsulation.None scopes `:host` to this
  // element's tag, so the rule is precise rather than global.
  encapsulation: ViewEncapsulation.None,
  styles: `
    hlm-dialog-content > *:not([data-slot='dialog-close']) {
      display: flex;
      flex-direction: column;
      min-height: 0;
      flex: 1 1 auto;
    }
  `,
  host: {
    'data-slot': 'dialog-content',
    '[attr.data-state]': 'state()',
  },
  template: `
    @if (component) {
      <ng-container [ngComponentOutlet]="component" />
    } @else {
      <ng-content />
    }

    @if (showCloseButton()) {
      <button hlmBtn variant="ghost" size="icon-sm" class="absolute end-2 top-2" hlmDialogClose>
        <span class="sr-only">close</span>
        <ng-icon name="lucideX" />
      </button>
    }
  `,
})
export class HlmDialogContent {
  private readonly _dialogRef = inject(BrnDialogRef);
  private readonly _dialogContext = injectBrnDialogContext<HlmDialogContentContext | null>({
    optional: true,
  });

  public readonly showCloseButton = input<boolean, BooleanInput>(
    this._dialogContext?.$showCloseButton ?? true,
    {
      transform: booleanAttribute,
    },
  );

  public readonly state = computed(() => this._dialogRef?.state() ?? 'closed');

  public readonly component = this._dialogContext?.$component;
  private readonly _dynamicComponentClass = this._dialogContext?.$dynamicComponentClass;

  constructor() {
    classes(() => [
      // A bounded flex column that clips: the header and footer are fixed
      // rows and only [hlmDialogBody] scrolls. No padding here — each row pads
      // itself, so the footer can span the full width without negative
      // margins, and no gap, so rows meet their own borders cleanly.
      'bg-popover text-popover-foreground data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95 ring-foreground/10 flex max-h-[85dvh] max-w-[calc(100%-2rem)] flex-col overflow-hidden rounded-xl text-sm ring-1 duration-100 sm:max-w-sm relative mx-auto w-full outline-none sm:mx-0',
      this._dynamicComponentClass,
    ]);
  }
}
