import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { GucInfo } from '../../api/models/guc-info';

export interface ConfigDialogContext {
  parameters: Record<string, string>;
  catalog: GucInfo[];
}

interface Row {
  name: string;
  value: string;
}

/** Edit a cluster's PostgreSQL settings. Resolves to the full parameter map,
 * or undefined on cancel. */
@Component({
  selector: 'pg-cluster-config-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    NgIcon,
    HlmButton,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
    HlmInput,
    HlmLabel,
  ],
  providers: [provideIcons({ lucidePlus, lucideTrash2 })],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>{{ 'config.editTitle' | transloco }}</h2>
    </div>

    <div class="grid max-h-[24rem] gap-3 overflow-y-auto py-2">
      @for (row of rows(); track row.name) {
        <div class="grid gap-1.5" [attr.data-testid]="'param-' + row.name">
          <div class="flex items-center justify-between gap-2">
            <label hlmLabel [for]="'param-input-' + row.name" class="font-mono text-xs">
              {{ row.name }}
            </label>
            <span class="flex items-center gap-2">
              <span
                class="rounded-full px-2 py-0.5 text-[11px] font-medium"
                [class.bg-secondary]="applyOf(row.name) === 'reload'"
                [class.text-secondary-foreground]="applyOf(row.name) === 'reload'"
                [class.bg-warning]="applyOf(row.name) === 'restart'"
                [class.text-background]="applyOf(row.name) === 'restart'"
              >
                {{
                  (applyOf(row.name) === 'restart' ? 'config.needsRestart' : 'config.appliesLive')
                    | transloco
                }}
              </span>
              <button
                hlmBtn
                variant="ghost"
                size="icon-sm"
                [attr.aria-label]="'config.remove' | transloco"
                [attr.data-testid]="'remove-' + row.name"
                (click)="remove(row.name)"
              >
                <ng-icon name="lucideTrash2" size="14" />
              </button>
            </span>
          </div>
          <input
            hlmInput
            [id]="'param-input-' + row.name"
            [attr.data-testid]="'input-' + row.name"
            [ngModel]="row.value"
            (ngModelChange)="setValue(row.name, $event)"
          />
          <p class="text-muted-foreground text-xs">
            {{ summaryOf(row.name) }} {{ 'config.defaultIs' | transloco: { value: defaultOf(row.name) } }}
          </p>
        </div>
      } @empty {
        <p class="text-muted-foreground text-sm">{{ 'config.noneSet' | transloco }}</p>
      }

      <div class="border-border grid gap-1.5 border-t pt-3">
        <label hlmLabel for="param-add">{{ 'config.addSetting' | transloco }}</label>
        <div class="flex gap-2">
          <select
            id="param-add"
            data-testid="param-select"
            class="border-input bg-background h-9 flex-1 rounded-md border px-3 text-sm"
            [ngModel]="selected()"
            (ngModelChange)="selected.set($event)"
          >
            <option value="">{{ 'config.choose' | transloco }}</option>
            @for (category of categories(); track category) {
              <optgroup [label]="category">
                @for (g of available(category); track g.name) {
                  <option [value]="g.name">{{ g.name }}</option>
                }
              </optgroup>
            }
          </select>
          <button
            hlmBtn
            variant="outline"
            data-testid="param-add"
            [disabled]="!selected()"
            (click)="add()"
          >
            <ng-icon name="lucidePlus" size="14" />
            {{ 'config.add' | transloco }}
          </button>
        </div>
      </div>
    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="config-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'config.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterConfigDialogComponent {
  protected readonly ref = inject<BrnDialogRef<Record<string, string> | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<ConfigDialogContext>();

  protected readonly catalog = this.ctx.catalog;
  protected readonly rows = signal<Row[]>(
    Object.entries(this.ctx.parameters)
      .map(([name, value]) => ({ name, value }))
      .sort((a, b) => a.name.localeCompare(b.name)),
  );
  protected readonly selected = signal('');

  protected readonly categories = computed(() => [
    ...new Set(this.catalog.map((g) => g.category)),
  ]);
  protected readonly valid = computed(() => this.rows().every((r) => r.value.trim().length > 0));

  protected available(category: string): GucInfo[] {
    const used = new Set(this.rows().map((r) => r.name));
    return this.catalog.filter((g) => g.category === category && !used.has(g.name));
  }

  protected applyOf(name: string): string {
    return this.catalog.find((g) => g.name === name)?.apply ?? 'reload';
  }
  protected summaryOf(name: string): string {
    return this.catalog.find((g) => g.name === name)?.summary ?? '';
  }
  protected defaultOf(name: string): string {
    return this.catalog.find((g) => g.name === name)?.default ?? '';
  }

  protected add(): void {
    const name = this.selected();
    if (!name || this.rows().some((r) => r.name === name)) {
      return;
    }
    const value = this.defaultOf(name);
    this.rows.update((rows) => [...rows, { name, value }].sort((a, b) => a.name.localeCompare(b.name)));
    this.selected.set('');
  }

  protected remove(name: string): void {
    this.rows.update((rows) => rows.filter((r) => r.name !== name));
  }

  protected setValue(name: string, value: string): void {
    this.rows.update((rows) => rows.map((r) => (r.name === name ? { ...r, value } : r)));
  }

  protected submit(): void {
    const out: Record<string, string> = {};
    for (const row of this.rows()) {
      out[row.name] = row.value.trim();
    }
    this.ref.close(out);
  }
}
