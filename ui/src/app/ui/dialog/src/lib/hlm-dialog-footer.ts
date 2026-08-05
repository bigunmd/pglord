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
        // The negative margins must match hlm-dialog-content's padding, or the
        // footer stops short of the dialog's edges. Sticky so the actions stay
        // reachable while a long form scrolls behind them; opaque for the same
        // reason (a translucent bar would show the scrolling text through it).
        'bg-muted -mx-6 -mb-6 sticky bottom-0 rounded-b-xl border-t p-4 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end',
    );
  }
}
