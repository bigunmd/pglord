import { Directive } from '@angular/core';
import { classes } from '@spartan-ng/helm/utils';

@Directive({
  selector: '[hlmDialogHeader],hlm-dialog-header',
  host: { 'data-slot': 'dialog-header' },
})
export class HlmDialogHeader {
  constructor() {
    classes(() => 'flex shrink-0 flex-col gap-1.5 px-6 pt-6 pb-4');
  }
}
