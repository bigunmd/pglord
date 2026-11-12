import { Component, inject, signal } from '@angular/core';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideCircleCheck, lucideCircleHelp, lucideCircleX } from '@ng-icons/lucide';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmTableImports } from '@spartan-ng/helm/table';

import { Api } from '../../api/api';
import { PreflightReport } from '../../api/models/preflight-report';
import { getPreflight } from '../../api/fn/system/get-preflight';
import { problemDetail } from '../../core/problem';

/** The Kubernetes API reports the server's permissions, not the operator's identity. */
@Component({
  selector: 'pg-preflight',
  imports: [TranslocoModule, NgIcon, HlmButton, HlmTableImports],
  providers: [provideIcons({ lucideCircleCheck, lucideCircleHelp, lucideCircleX })],
  template: `
    <div class="pg-page">
      <header class="pg-page-header">
        <div>
          <h1 class="pg-page-title">{{ 'preflight.title' | transloco }}</h1>
          <p class="pg-page-description">{{ 'preflight.intro' | transloco }}</p>
        </div>
        <button
          hlmBtn
          variant="outline"
          [disabled]="loading()"
          (click)="load()"
          data-testid="retry-preflight"
        >
          {{ (loading() ? 'common.loading' : 'ux.permissions.recheck') | transloco }}
        </button>
      </header>

      @if (error(); as err) {
        <div class="pg-alert mb-6" role="alert">
          <p class="font-semibold">{{ 'ux.permissions.errorTitle' | transloco }}</p>
          <p class="mt-1 text-sm">{{ err }}</p>
          @if (report()) {
            <p class="mt-2 text-sm">{{ 'ux.permissions.stale' | transloco }}</p>
          }
        </div>
      }
      @if (loading() && !report()) {
        <div class="pg-empty" role="status">{{ 'common.loading' | transloco }}</div>
      }
      @if (report(); as r) {
        <section
          class="pg-panel mb-6 p-5"
          data-testid="preflight-banner"
          aria-labelledby="permission-result"
        >
          <div class="flex items-start gap-3">
            <ng-icon
              [name]="r.ok ? 'lucideCircleCheck' : 'lucideCircleX'"
              size="22"
              class="mt-0.5 shrink-0"
              [class.text-success]="r.ok"
              [class.text-destructive]="!r.ok"
              aria-hidden="true"
            />
            <div class="min-w-0">
              <h2 id="permission-result" class="break-words font-semibold">
                @if (r.ok) {
                  {{ 'preflight.allGranted' | transloco: { namespace: r.namespace } }}
                } @else {
                  {{
                    'preflight.missing' | transloco: { count: r.missing, namespace: r.namespace }
                  }}
                }
              </h2>
              <p class="text-muted-foreground mt-2 max-w-3xl text-sm leading-relaxed">
                {{
                  (r.ok ? 'ux.permissions.successHint' : 'ux.permissions.missingHint') | transloco
                }}
              </p>
            </div>
          </div>
        </section>

        <section class="pg-panel" aria-labelledby="checks-title" [attr.aria-busy]="loading()">
          <div class="pg-section-header">
            <h2 id="checks-title" class="font-semibold">
              {{ 'ux.permissions.checks' | transloco }}
            </h2>
            <p class="text-muted-foreground mt-1 text-sm">
              {{ 'ux.permissions.checksHint' | transloco }}
            </p>
          </div>
          @if (r.checks.length === 0) {
            <p class="pg-empty">{{ 'ux.permissions.empty' | transloco }}</p>
          } @else {
            <div class="overflow-x-auto">
              <table hlmTable class="w-full">
                <caption class="sr-only">
                  {{
                    'ux.permissions.checks' | transloco
                  }}
                </caption>
                <thead hlmTHead>
                  <tr hlmTr>
                    <th hlmTh scope="col">{{ 'preflight.resource' | transloco }}</th>
                    <th hlmTh scope="col">{{ 'preflight.verb' | transloco }}</th>
                    <th hlmTh scope="col">{{ 'preflight.state' | transloco }}</th>
                  </tr>
                </thead>
                <tbody hlmTBody>
                  @for (c of r.checks; track c.resource + '/' + c.verb) {
                    <tr hlmTr>
                      <td hlmTd class="max-w-48 whitespace-normal break-all font-mono text-xs">
                        {{ c.resource }}
                      </td>
                      <td hlmTd class="font-mono text-xs">{{ c.verb }}</td>
                      <td hlmTd class="whitespace-normal">
                        <span
                          class="pg-status"
                          [class.text-success]="c.allowed === true"
                          [class.text-destructive]="c.allowed === false"
                          [class.text-warning]="c.allowed !== true && c.allowed !== false"
                        >
                          <ng-icon
                            [name]="
                              c.allowed === true
                                ? 'lucideCircleCheck'
                                : c.allowed === false
                                  ? 'lucideCircleX'
                                  : 'lucideCircleHelp'
                            "
                            size="14"
                            aria-hidden="true"
                          />
                          {{
                            (c.allowed === true
                              ? 'preflight.granted'
                              : c.allowed === false
                                ? 'preflight.denied'
                                : 'preflight.unknown'
                            ) | transloco
                          }}
                        </span>
                        @if (c.reason && c.allowed !== true) {
                          <p
                            class="text-muted-foreground mt-2 max-w-xl break-words text-xs leading-relaxed"
                          >
                            {{ c.reason }}
                          </p>
                        }
                      </td>
                    </tr>
                  }
                </tbody>
              </table>
            </div>
          }
        </section>
      }
    </div>
  `,
})
export class PreflightComponent {
  private readonly api = inject(Api);
  protected readonly report = signal<PreflightReport | null>(null);
  protected readonly error = signal<string | null>(null);
  protected readonly loading = signal(false);
  private pending: Promise<void> | null = null;

  constructor() {
    void this.load();
  }

  protected load(): Promise<void> {
    if (this.pending) return this.pending;
    this.loading.set(true);
    this.pending = this.api
      .invoke(getPreflight)
      .then((report) => {
        this.report.set(report);
        this.error.set(null);
      })
      .catch((err) => this.error.set(problemDetail(err)))
      .finally(() => {
        this.loading.set(false);
        this.pending = null;
      });
    return this.pending;
  }
}
