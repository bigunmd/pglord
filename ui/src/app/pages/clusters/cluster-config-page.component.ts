import { Component, computed, inject, input, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { Api } from '../../api/api';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { patchCluster } from '../../api/fn/clusters/patch-cluster';
import { listGucs } from '../../api/fn/system/list-gucs';
import { GucInfo } from '../../api/models/guc-info';
import { problemDetail } from '../../core/problem';
import { BreadcrumbsComponent } from '../../shell/breadcrumbs.component';

interface Row {
  name: string;
  value: string;
}

/** Edit a cluster's PostgreSQL settings. A curated catalog plus a growing list
 * of overrides needs more room than a dialog, so it gets its own URL. */
@Component({
  selector: 'pg-cluster-config-page',
  imports: [
    FormsModule,
    TranslocoModule,
    NgIcon,
    HlmButton,
    HlmInput,
    HlmLabel,
    BreadcrumbsComponent,
  ],
  providers: [provideIcons({ lucidePlus, lucideTrash2 })],
  template: `
    <div class="pg-form-page">
      <pg-breadcrumbs [crumbs]="crumbs()" />
      <h1 class="pg-page-title">{{ 'config.editTitle' | transloco }}</h1>
      <p class="pg-page-description mb-6">{{ 'config.intro' | transloco }}</p>

      @if (error(); as err) {
        <p class="pg-alert text-destructive mb-4" role="alert">{{ err }}</p>
      }

      @if (loading()) {
        <p class="pg-panel text-muted-foreground text-sm" role="status">
          {{ 'common.loading' | transloco }}
        </p>
      } @else {
        <div class="grid gap-5">
          @for (row of rows(); track row.name) {
            <div class="pg-form-section !my-0 grid gap-3" [attr.data-testid]="'param-' + row.name">
              <div class="flex items-center justify-between gap-3">
                <label hlmLabel [for]="'param-input-' + row.name" class="font-mono text-sm">
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
                      (applyOf(row.name) === 'restart'
                        ? 'config.needsRestart'
                        : 'config.appliesLive'
                      ) | transloco
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
              <!-- Value beside its guidance: the input needs little width, the
                   explanation needs a readable measure. -->
              <div class="grid gap-2 sm:grid-cols-[16rem_1fr] sm:items-start sm:gap-4">
                <input
                  hlmInput
                  [id]="'param-input-' + row.name"
                  [attr.data-testid]="'input-' + row.name"
                  [ngModel]="row.value"
                  (ngModelChange)="setValue(row.name, $event)"
                />
                <p class="text-muted-foreground text-xs leading-relaxed sm:pt-2">
                  {{ summaryOf(row.name) }}
                  {{ 'config.defaultIs' | transloco: { value: defaultOf(row.name) } }}
                </p>
              </div>
            </div>
          } @empty {
            <p class="text-muted-foreground py-2 text-sm">{{ 'config.noneSet' | transloco }}</p>
          }

          <div class="border-border grid gap-2 border-t pt-5">
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
      }
      <div class="pg-form-actions">
        <div>
          <div class="ml-auto flex gap-2">
            <button hlmBtn variant="ghost" (click)="cancel()">
              {{ 'common.cancel' | transloco }}
            </button>
            <button
              hlmBtn
              data-testid="config-submit"
              [disabled]="loading() || busy() || !valid()"
              (click)="submit()"
            >
              {{ (busy() ? 'ux.forms.saving' : 'config.apply') | transloco }}
            </button>
          </div>
        </div>
      </div>
    </div>
  `,
})
export class ClusterConfigPageComponent {
  private readonly api = inject(Api);
  private readonly router = inject(Router);

  public readonly ns = input.required<string>();
  public readonly name = input.required<string>();

  protected readonly loading = signal(true);
  protected readonly busy = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly crumbs = computed(() => [
    { label: 'Clusters', link: '/clusters' },
    { label: this.name(), link: '/namespaces/' + this.ns() + '/clusters/' + this.name() },
    { label: 'Configuration' },
  ]);

  protected readonly catalog = signal<GucInfo[]>([]);
  protected readonly rows = signal<Row[]>([]);
  protected readonly selected = signal('');

  protected readonly categories = computed(() => [
    ...new Set(this.catalog().map((g) => g.category)),
  ]);
  protected readonly valid = computed(() => this.rows().every((r) => r.value.trim().length > 0));

  constructor() {
    queueMicrotask(() => void this.load());
  }

  /** The catalog and the current overrides are both needed before anything can
   * be edited, so the page waits for both. */
  private async load(): Promise<void> {
    try {
      const [d, catalog] = await Promise.all([
        this.api.invoke(getCluster, { ns: this.ns(), name: this.name() }),
        this.api.invoke(listGucs),
      ]);
      this.catalog.set(catalog);
      this.rows.set(
        Object.entries(d.parameters ?? {})
          .map(([name, value]) => ({ name, value }))
          .sort((a, b) => a.name.localeCompare(b.name)),
      );
      this.error.set(null);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.loading.set(false);
    }
  }

  protected available(category: string): GucInfo[] {
    const used = new Set(this.rows().map((r) => r.name));
    return this.catalog().filter((g) => g.category === category && !used.has(g.name));
  }

  protected applyOf(name: string): string {
    return this.catalog().find((g) => g.name === name)?.apply ?? 'reload';
  }
  protected summaryOf(name: string): string {
    return this.catalog().find((g) => g.name === name)?.summary ?? '';
  }
  protected defaultOf(name: string): string {
    return this.catalog().find((g) => g.name === name)?.default ?? '';
  }

  protected add(): void {
    const name = this.selected();
    if (!name || this.rows().some((r) => r.name === name)) {
      return;
    }
    const value = this.defaultOf(name);
    this.rows.update((rows) =>
      [...rows, { name, value }].sort((a, b) => a.name.localeCompare(b.name)),
    );
    this.selected.set('');
  }

  protected remove(name: string): void {
    this.rows.update((rows) => rows.filter((r) => r.name !== name));
  }

  protected setValue(name: string, value: string): void {
    this.rows.update((rows) => rows.map((r) => (r.name === name ? { ...r, value } : r)));
  }

  protected cancel(): void {
    void this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
  }

  protected async submit(): Promise<void> {
    const parameters: Record<string, string> = {};
    for (const row of this.rows()) {
      parameters[row.name] = row.value.trim();
    }

    this.busy.set(true);
    this.error.set(null);
    try {
      await this.api.invoke(patchCluster, {
        ns: this.ns(),
        name: this.name(),
        body: { parameters },
      });
      await this.router.navigate(['/namespaces', this.ns(), 'clusters', this.name()]);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.busy.set(false);
    }
  }
}
