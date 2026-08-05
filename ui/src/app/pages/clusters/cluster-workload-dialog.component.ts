import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucidePlus, lucideTrash2 } from '@ng-icons/lucide';
import { BrnDialogRef, injectBrnDialogContext } from '@spartan-ng/brain/dialog';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmDialogBody, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle } from '@spartan-ng/helm/dialog';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { WorkloadSpec } from '../../api/models/workload-spec';

/** Kubernetes quantity: 500m, 2, 1Gi, 512Mi. Mirrors the server's check. */
const QUANTITY_RE = /^[0-9]+(\.[0-9]+)?(m|k|M|G|T|Ki|Mi|Gi|Ti)?$/;
const RESERVED_PREFIXES = ['app.kubernetes.io/', 'pglord.dev/', 'statefulset.kubernetes.io/'];

interface KeyValue {
  key: string;
  value: string;
}

/** Edit placement and resources. Resolves to the workload block, or undefined. */
@Component({
  selector: 'pg-cluster-workload-dialog',
  imports: [
    FormsModule,
    TranslocoModule,
    NgIcon,
    HlmButton,
    HlmDialogBody,
    HlmDialogFooter,
    HlmDialogHeader,
    HlmDialogTitle,
    HlmInput,
    HlmLabel,
  ],
  providers: [provideIcons({ lucidePlus, lucideTrash2 })],
  template: `
    <!-- A rule under the title separates chrome from content, matching the
         dividers between sections below. -->
    <div hlmDialogHeader class="border-border border-b pb-4">
      <h2 hlmDialogTitle>{{ 'workload.editTitle' | transloco }}</h2>
    </div>

    <div hlmDialogBody>
  <!-- Viewport-relative so the form uses tall screens instead of a fixed box.
           pg-scroll reserves the scrollbar gutter so nothing is clipped by it. -->
      <div class="pg-scroll grid max-h-[min(65vh,34rem)] gap-6 overflow-y-auto py-1 pr-2">
        <div class="grid gap-2">
          <span hlmLabel>{{ 'workload.antiAffinity' | transloco }}</span>
          <div class="bg-muted grid grid-cols-3 gap-1 rounded-lg p-1" role="tablist">
            @for (mode of modes; track mode) {
              <button
                type="button"
                class="cursor-pointer rounded-md px-2 py-1.5 text-sm font-medium transition-colors"
                [class.bg-background]="antiAffinity() === mode"
                [class.shadow-sm]="antiAffinity() === mode"
                [class.text-muted-foreground]="antiAffinity() !== mode"
                role="tab"
                [attr.aria-selected]="antiAffinity() === mode"
                [attr.data-testid]="'anti-affinity-' + mode"
                (click)="antiAffinity.set(mode)"
              >
                {{ 'workload.antiAffinityModes.' + mode | transloco }}
              </button>
            }
          </div>
          <p class="text-muted-foreground min-h-9 text-xs leading-relaxed">
            {{ 'workload.antiAffinityHelp.' + antiAffinity() | transloco }}
          </p>
        </div>

        <div class="border-border grid gap-3 border-t pt-5">
          <span hlmLabel>{{ 'workload.resources' | transloco }}</span>
          <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
            @for (f of resourceFields; track f.key) {
              <div class="grid gap-2">
                <label hlmLabel [for]="'res-' + f.key">{{ 'workload.' + f.key | transloco }}</label>
                <input
                  hlmInput
                  [id]="'res-' + f.key"
                  [attr.data-testid]="'res-' + f.key"
                  [placeholder]="f.placeholder"
                  [ngModel]="resources()[f.key]"
                  (ngModelChange)="setResource(f.key, $event)"
                />
              </div>
            }
          </div>
          <p
            class="text-xs leading-relaxed"
            [class.text-destructive]="!resourcesValid()"
            [class.text-muted-foreground]="resourcesValid()"
          >
            {{ 'workload.resourcesHelp' | transloco }}
          </p>
        </div>

        @for (group of groups; track group.kind) {
          <div class="border-border grid gap-3 border-t pt-5">
            <span hlmLabel>{{ 'workload.' + group.kind | transloco }}</span>
            @for (row of rowsOf(group.kind); track $index) {
              <div class="flex items-center gap-3">
                <input
                  hlmInput
                  class="flex-1"
                  [attr.data-testid]="group.kind + '-key-' + $index"
                  [placeholder]="'workload.keyPlaceholder' | transloco"
                  [ngModel]="row.key"
                  (ngModelChange)="setRow(group.kind, $index, $event, row.value)"
                />
                <input
                  hlmInput
                  class="flex-1"
                  [placeholder]="'workload.valuePlaceholder' | transloco"
                  [ngModel]="row.value"
                  (ngModelChange)="setRow(group.kind, $index, row.key, $event)"
                />
                <button
                  hlmBtn
                  variant="ghost"
                  size="icon-sm"
                  [attr.aria-label]="'workload.removeRow' | transloco"
                  (click)="removeRow(group.kind, $index)"
                >
                  <ng-icon name="lucideTrash2" size="14" />
                </button>
              </div>
            }
            <button
              hlmBtn
              variant="outline"
              size="sm"
              class="justify-self-start"
              [attr.data-testid]="'add-' + group.kind"
              (click)="addRow(group.kind)"
            >
              <ng-icon name="lucidePlus" size="14" />
              {{ 'workload.addRow' | transloco }}
            </button>
          </div>
        }

        @if (reservedUsed(); as bad) {
          <p class="text-destructive text-xs" role="alert">
            {{ 'workload.reservedKey' | transloco: { key: bad } }}
          </p>
        }
      </div>

    </div>

    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(undefined)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="workload-submit" [disabled]="!valid()" (click)="submit()">
        {{ 'workload.apply' | transloco }}
      </button>
    </div>
  `,
})
export class ClusterWorkloadDialogComponent {
  protected readonly ref = inject<BrnDialogRef<WorkloadSpec | undefined>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ workload: WorkloadSpec }>();

  protected readonly modes = ['preferred', 'required', 'disabled'] as const;
  protected readonly resourceFields = [
    { key: 'cpu_request' as const, placeholder: '100m' },
    { key: 'cpu_limit' as const, placeholder: '1' },
    { key: 'memory_request' as const, placeholder: '256Mi' },
    { key: 'memory_limit' as const, placeholder: '1Gi' },
  ];
  protected readonly groups = [{ kind: 'labels' as const }, { kind: 'annotations' as const }];

  protected readonly antiAffinity = signal<'preferred' | 'required' | 'disabled'>(
    this.ctx.workload.anti_affinity ?? 'preferred',
  );
  protected readonly resources = signal<Record<string, string>>({
    cpu_request: this.ctx.workload.resources?.cpu_request ?? '',
    cpu_limit: this.ctx.workload.resources?.cpu_limit ?? '',
    memory_request: this.ctx.workload.resources?.memory_request ?? '',
    memory_limit: this.ctx.workload.resources?.memory_limit ?? '',
  });
  protected readonly labels = signal<KeyValue[]>(toRows(this.ctx.workload.labels));
  protected readonly annotations = signal<KeyValue[]>(toRows(this.ctx.workload.annotations));

  protected readonly resourcesValid = computed(() =>
    Object.values(this.resources()).every((v) => !v.trim() || QUANTITY_RE.test(v.trim())),
  );
  protected readonly reservedUsed = computed(() => {
    const keys = [...this.labels(), ...this.annotations()].map((r) => r.key.trim());
    return keys.find((k) => RESERVED_PREFIXES.some((p) => k.startsWith(p)));
  });
  protected readonly valid = computed(() => this.resourcesValid() && !this.reservedUsed());

  protected rowsOf(kind: 'labels' | 'annotations'): KeyValue[] {
    return kind === 'labels' ? this.labels() : this.annotations();
  }

  protected setResource(key: string, value: string): void {
    this.resources.update((r) => ({ ...r, [key]: value }));
  }

  protected addRow(kind: 'labels' | 'annotations'): void {
    const target = kind === 'labels' ? this.labels : this.annotations;
    target.update((rows) => [...rows, { key: '', value: '' }]);
  }

  protected removeRow(kind: 'labels' | 'annotations', index: number): void {
    const target = kind === 'labels' ? this.labels : this.annotations;
    target.update((rows) => rows.filter((_, i) => i !== index));
  }

  protected setRow(
    kind: 'labels' | 'annotations',
    index: number,
    key: string,
    value: string,
  ): void {
    const target = kind === 'labels' ? this.labels : this.annotations;
    target.update((rows) => rows.map((r, i) => (i === index ? { key, value } : r)));
  }

  protected submit(): void {
    const res = this.resources();
    const pick = (k: string) => (res[k]?.trim() ? res[k].trim() : undefined);
    this.ref.close({
      ...this.ctx.workload,
      anti_affinity: this.antiAffinity(),
      resources: {
        cpu_request: pick('cpu_request'),
        cpu_limit: pick('cpu_limit'),
        memory_request: pick('memory_request'),
        memory_limit: pick('memory_limit'),
      },
      labels: toMap(this.labels()),
      annotations: toMap(this.annotations()),
    });
  }
}

function toRows(map: Record<string, string> | undefined): KeyValue[] {
  return Object.entries(map ?? {}).map(([key, value]) => ({ key, value }));
}

function toMap(rows: KeyValue[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) {
    if (r.key.trim()) {
      out[r.key.trim()] = r.value.trim();
    }
  }
  return out;
}
