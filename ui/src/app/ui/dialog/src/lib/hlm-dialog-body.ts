import { Directive } from '@angular/core';
import { classes } from '@spartan-ng/helm/utils';

/**
 * The scrolling middle of a dialog, between the fixed header and footer.
 *
 * `min-h-0` is load-bearing: a flex child defaults to `min-height: auto`, which
 * refuses to shrink below its content, so without it the body pushes the
 * dialog past its max-height and nothing scrolls.
 */
@Directive({
  selector: '[hlmDialogBody],hlm-dialog-body',
  host: { 'data-slot': 'dialog-body' },
})
export class HlmDialogBody {
  constructor() {
    classes(() => 'min-h-0 flex-1 overflow-y-auto overscroll-contain px-6 py-1');
  }
}
