import { Directive } from '@angular/core';
import { classes } from '@spartan-ng/helm/utils';

@Directive({
  selector: '[hlmDialogFooter],hlm-dialog-footer',
  host: { 'data-slot': 'dialog-footer' },
})
export class HlmDialogFooter {
  constructor() {
    classes(
      () =>
        // No negative margins and no sticky: the content is a flex column with
        // no padding, so the footer is simply the last fixed row and spans the
        // full width by construction.
        'bg-muted flex shrink-0 flex-col-reverse gap-2 border-t px-6 py-4 sm:flex-row sm:justify-end',
    );
  }
}
