import { Component, inject, signal } from '@angular/core';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmCardImports } from '@spartan-ng/helm/card';

import { Api } from '../../api/api';
import { version } from '../../api/fn/system/version';

type ServerStatus = 'checking' | 'reachable' | 'unreachable';

@Component({
  selector: 'pg-home',
  imports: [TranslocoModule, HlmCardImports],
  template: `
    <section hlmCard class="max-w-md">
      <div hlmCardHeader>
        <h2 hlmCardTitle>{{ 'home.serverStatus' | transloco }}</h2>
        <p hlmCardDescription>
          @switch (status()) {
            @case ('checking') {
              {{ 'home.checking' | transloco }}
            }
            @case ('reachable') {
              <span class="text-success">{{ 'home.reachable' | transloco }}</span>
            }
            @case ('unreachable') {
              <span class="text-destructive">{{ 'home.unreachable' | transloco }}</span>
            }
          }
        </p>
      </div>
      @if (serverVersion(); as v) {
        <div hlmCardContent class="text-sm">
          {{ 'home.version' | transloco }}: <span class="font-mono">{{ v }}</span>
        </div>
      }
    </section>
  `,
})
export class HomeComponent {
  private readonly api = inject(Api);

  protected readonly status = signal<ServerStatus>('checking');
  protected readonly serverVersion = signal<string | null>(null);

  constructor() {
    this.api
      .invoke(version)
      .then((v) => {
        this.serverVersion.set(v.version);
        this.status.set('reachable');
      })
      .catch(() => this.status.set('unreachable'));
  }
}
