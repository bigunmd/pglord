import { Component, inject, signal } from '@angular/core';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideCircleCheck, lucideCircleHelp, lucideCircleX } from '@ng-icons/lucide';
import { HlmTableImports } from '@spartan-ng/helm/table';

import { Api } from '../../api/api';
import { PreflightReport } from '../../api/models/preflight-report';
import { getPreflight } from '../../api/fn/system/get-preflight';

/** Shows which cluster permissions pglord actually holds, as reported by the
 * Kubernetes apiserver itself. */
@Component({
  selector: 'pg-preflight',
  imports: [TranslocoModule, NgIcon, HlmTableImports],
  providers: [provideIcons({ lucideCircleCheck, lucideCircleHelp, lucideCircleX })],
  template: `
    <div class="mx-auto max-w-4xl">
      <h1 class="mb-1 text-lg font-semibold">{{ 'preflight.title' | transloco }}</h1>
      <p class="text-muted-foreground mb-4 text-sm">{{ 'preflight.intro' | transloco }}</p>

      @if (error(); as err) {
        <p class="text-destructive text-sm" role="alert">{{ err }}</p>
      }

      @if (report(); as r) {
        <div
          class="mb-4 flex items-center gap-2 rounded-lg border p-3 text-sm"
          [class.border-success]="r.ok"
          [class.border-destructive]="!r.ok"
          data-testid="preflight-banner"
        >
          <ng-icon
            [name]="r.ok ? 'lucideCircleCheck' : 'lucideCircleX'"
            size="18"
            [class.text-success]="r.ok"
            [class.text-destructive]="!r.ok"
          />
          <span>
            @if (r.ok) {
              {{ 'preflight.allGranted' | transloco: { namespace: r.namespace } }}
            } @else {
              {{ 'preflight.missing' | transloco: { count: r.missing, namespace: r.namespace } }}
            }
          </span>
        </div>

        <div class="border-border overflow-x-auto rounded-lg border">
          <table hlmTable class="w-full">
            <thead hlmTHead>
              <tr hlmTr>
                <th hlmTh>{{ 'preflight.resource' | transloco }}</th>
                <th hlmTh>{{ 'preflight.verb' | transloco }}</th>
                <th hlmTh>{{ 'preflight.state' | transloco }}</th>
              </tr>
            </thead>
            <tbody hlmTBody>
              @for (c of r.checks; track c.resource + c.verb) {
                <tr hlmTr>
                  <td hlmTd class="font-mono text-xs">{{ c.resource }}</td>
                  <td hlmTd class="font-mono text-xs">{{ c.verb }}</td>
                  <td hlmTd>
                    <span class="inline-flex items-center gap-1.5 text-sm">
                      <ng-icon
                        [name]="
                          c.allowed === true
                            ? 'lucideCircleCheck'
                            : c.allowed === false
                              ? 'lucideCircleX'
                              : 'lucideCircleHelp'
                        "
                        size="14"
                        [class.text-success]="c.allowed === true"
                        [class.text-destructive]="c.allowed === false"
                        [class.text-warning]="c.allowed === null"
                      />
                      {{
                        (c.allowed === true
                          ? 'preflight.granted'
                          : c.allowed === false
                            ? 'preflight.denied'
                            : 'preflight.unknown'
                        ) | transloco
                      }}
                      @if (c.reason && c.allowed !== true) {
                        <span class="text-muted-foreground text-xs">— {{ c.reason }}</span>
                      }
                    </span>
                  </td>
                </tr>
              }
            </tbody>
          </table>
        </div>
      } @else if (!error()) {
        <p class="text-muted-foreground text-sm">{{ 'common.loading' | transloco }}</p>
      }
    </div>
  `,
})
export class PreflightComponent {
  private readonly api = inject(Api);

  protected readonly report = signal<PreflightReport | null>(null);
  protected readonly error = signal<string | null>(null);

  constructor() {
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      this.report.set(await this.api.invoke(getPreflight));
      this.error.set(null);
    } catch (err) {
      const e = err as { error?: { detail?: string; code?: string }; message?: string };
      this.error.set(e?.error?.detail ?? e?.error?.code ?? e?.message ?? 'Request failed');
    }
  }
}
