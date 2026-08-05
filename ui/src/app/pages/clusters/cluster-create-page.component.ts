import { Component, computed, inject, input, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { Router } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { HlmButton } from '@spartan-ng/helm/button';
import { HlmInput } from '@spartan-ng/helm/input';
import { HlmLabel } from '@spartan-ng/helm/label';

import { Api } from '../../api/api';
import { createCluster } from '../../api/fn/clusters/create-cluster';
import { listAllClusters } from '../../api/fn/clusters/list-all-clusters';
import { version } from '../../api/fn/system/version';
import { ClusterSpec } from '../../api/models/cluster-spec';
import { problemDetail } from '../../core/problem';
import { BreadcrumbsComponent } from '../../shell/breadcrumbs.component';
import { EnvEditorComponent, EnvRow, rowsToSpec, rowsValid } from './env-editor.component';

/** Mirrors the server's RFC1123 rule (crates/pglord-core/src/spec.rs). */
const NAME_RE = /^[a-z0-9]([-a-z0-9]{0,38}[a-z0-9])?$/;
const STORAGE_RE = /^[0-9]+(Ki|Mi|Gi|Ti)$/;
const REPLICA_CHOICES = [1, 2, 3, 4, 5] as const;

/**
 * Creating a cluster is a multi-section form, not a single decision, so it
 * gets a page of its own: room to breathe, a shareable URL, and browser back.
 */
@Component({
  selector: 'pg-cluster-create-page',
  imports: [
    FormsModule,
    TranslocoModule,
    HlmButton,
    HlmInput,
    HlmLabel,
    EnvEditorComponent,
    BreadcrumbsComponent,
  ],
  template: `
    <div class="mx-auto max-w-2xl pb-24">
      <pg-breadcrumbs [crumbs]="crumbs()" />
      <h1 class="mb-1 text-lg font-semibold">{{ 'clusters.createTitle' | transloco }}</h1>
      <p class="text-muted-foreground mb-6 text-sm">{{ 'clusters.createIntro' | transloco }}</p>

      @if (error(); as err) {
        <p class="text-destructive mb-4 text-sm" role="alert">{{ err }}</p>
      }

      <div class="grid gap-6">
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-namespace">{{ 'clusters.namespace' | transloco }}</label>
        @if (namespaceFixed()) {
          <!-- The server manages exactly one namespace, so there is nothing to
               choose: show which one rather than offering a field that can
               only be filled in wrongly. -->
          <p
            class="border-border bg-muted/40 text-muted-foreground rounded-md border px-3 py-2 font-mono text-sm"
            data-testid="cluster-namespace-fixed"
          >
            {{ namespace() || '…' }}
          </p>
        } @else {
          <input
            hlmInput
            id="pg-cluster-namespace"
            class="font-mono"
            data-testid="cluster-namespace"
            list="pg-known-namespaces"
            [ngModel]="namespace()"
            (ngModelChange)="namespace.set($event)"
            autocomplete="off"
          />
          <datalist id="pg-known-namespaces">
            @for (n of knownNamespaces(); track n) {
              <option [value]="n"></option>
            }
          </datalist>
        }
        <p class="text-muted-foreground text-xs">
          {{ (namespaceFixed() ? 'clusters.namespaceFixedHelp' : 'clusters.namespaceHelp') | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-name">{{ 'clusters.name' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-name"
          data-testid="cluster-name"
          [ngModel]="name()"
          (ngModelChange)="name.set($event)"
          placeholder="orders-db"
          autocomplete="off"
        />
        <!-- Persistent helper text: the same string is always rendered and only
             recolored on error, so nothing reflows regardless of wrapping. -->
        <p
          class="text-xs"
          [class.text-muted-foreground]="!name() || nameValid()"
          [class.text-destructive]="name() && !nameValid()"
        >
          {{ 'clusters.nameHelp' | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <span hlmLabel>{{ 'clusters.replicas' | transloco }}</span>
        <div class="bg-muted grid grid-cols-5 gap-1 rounded-lg p-1" role="tablist">
          @for (n of replicaChoices; track n) {
            <button
              type="button"
              class="cursor-pointer rounded-md px-3 py-1.5 font-mono text-sm font-medium transition-colors"
              [class.bg-background]="replicas() === n"
              [class.shadow-sm]="replicas() === n"
              [class.text-muted-foreground]="replicas() !== n"
              role="tab"
              [attr.aria-selected]="replicas() === n"
              (click)="replicas.set(n)"
            >
              {{ n }}
            </button>
          }
        </div>
        <p class="text-muted-foreground text-xs">{{ 'clusters.replicasHint' | transloco }}</p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-storage">{{ 'clusters.storage' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-storage"
          [ngModel]="storage()"
          (ngModelChange)="storage.set($event)"
          placeholder="1Gi"
        />
        <p
          class="text-xs"
          [class.text-muted-foreground]="!storage() || storageValid()"
          [class.text-destructive]="storage() && !storageValid()"
        >
          {{ 'clusters.storageHelp' | transloco }}
        </p>
      </div>
      <div class="grid gap-1.5">
        <label hlmLabel for="pg-cluster-image">{{ 'clusters.image' | transloco }}</label>
        <input
          hlmInput
          id="pg-cluster-image"
          [ngModel]="image()"
          (ngModelChange)="image.set($event)"
          [placeholder]="'clusters.imageDefault' | transloco"
        />
        <p class="text-muted-foreground text-xs leading-relaxed">
          {{ 'clusters.imageHelp' | transloco }}
        </p>
      </div>

      <!-- Collapsed by default: most clusters need none of this, but init
           scripts and their environment can only be set here, since they run
           when the data directory is first created. -->
      <div class="border-border border-t pt-4">
        <button
          hlmBtn
          variant="ghost"
          size="sm"
          class="-ml-2"
          data-testid="toggle-advanced"
          [attr.aria-expanded]="advanced()"
          (click)="advanced.set(!advanced())"
        >
          {{ (advanced() ? 'clusters.hideAdvanced' : 'clusters.showAdvanced') | transloco }}
        </button>
      </div>

      @if (advanced()) {
        <div class="grid gap-6">
          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-creds">
              {{ 'secrets.credentialsSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-creds"
              data-testid="create-credentials-secret"
              [placeholder]="(name() || 'orders-db') + '-credentials'"
              [ngModel]="credentialsSecret()"
              (ngModelChange)="credentialsSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.credentialsSecretHelp' | transloco }}
            </p>
          </div>

          <pg-env-editor [(rows)]="rows" />

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-init-cm">
              {{ 'secrets.initConfigMap' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-init-cm"
              data-testid="create-init-configmap"
              [ngModel]="initConfigMap()"
              (ngModelChange)="initConfigMap.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.initHelp' | transloco }}
            </p>
          </div>

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-init-secret">
              {{ 'secrets.initSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-init-secret"
              data-testid="create-init-secret"
              [ngModel]="initSecret()"
              (ngModelChange)="initSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'secrets.initSecretHelp' | transloco }}
            </p>
          </div>

          <div class="grid gap-1.5">
            <label hlmLabel for="pg-cluster-tls-secret">
              {{ 'tls.serverSecret' | transloco }}
            </label>
            <input
              hlmInput
              id="pg-cluster-tls-secret"
              data-testid="create-tls-secret"
              [placeholder]="'tls.serverSecretPlaceholder' | transloco"
              [ngModel]="tlsSecret()"
              (ngModelChange)="tlsSecret.set($event)"
            />
            <p class="text-muted-foreground text-xs leading-relaxed">
              {{ 'tls.serverSecretHelp' | transloco }}
            </p>
          </div>

          <label hlmLabel class="flex items-center gap-2">
            <input
              type="checkbox"
              class="accent-primary"
              data-testid="create-pooler-enabled"
              [ngModel]="poolerEnabled()"
              (ngModelChange)="poolerEnabled.set($event)"
            />
            {{ 'pooler.enable' | transloco }}
          </label>
          <p class="text-muted-foreground -mt-2 text-xs leading-relaxed">
            {{ 'pooler.explainer' | transloco }}
          </p>
        </div>
        }
      </div>
    </div>

    <!-- Sticky action bar: on a long form the primary action must stay
         reachable without scrolling back down. -->
    <div class="bg-background/95 border-border fixed inset-x-0 bottom-0 border-t backdrop-blur">
      <div class="mx-auto flex max-w-2xl items-center gap-3 px-4 py-3">
        <!-- A disabled button with no stated reason reads as a broken form: the
             name field's placeholder looks like a filled-in value, so an empty
             name silently deactivates submit. Say what is missing. -->
        @if (!valid()) {
          <p class="text-muted-foreground text-xs" data-testid="cluster-submit-blocked">
            {{ blockedReason() | transloco }}
          </p>
        }
        <div class="ml-auto flex gap-2">
          <button hlmBtn variant="ghost" (click)="cancel()">
            {{ 'common.cancel' | transloco }}
          </button>
          <button
            hlmBtn
            data-testid="cluster-submit"
            [disabled]="!valid() || busy()"
            (click)="submit()"
          >
            {{ 'clusters.create' | transloco }}
          </button>
        </div>
      </div>
    </div>
  `,
})
export class ClusterCreatePageComponent {
  private readonly api = inject(Api);
  private readonly router = inject(Router);

  /** Namespace to create in; supplied as a route query param by the list. */
  public readonly ns = input('');
  /** Where the cluster will be created. Seeded from the route when the list
   * had a namespace selected, otherwise from the server. */
  protected readonly namespace = signal('');
  /** A single-namespace server offers no choice, so the field is read-only. */
  protected readonly namespaceFixed = signal(true);
  protected readonly knownNamespaces = signal<string[]>([]);

  constructor() {
    queueMicrotask(() => void this.loadNamespaces());
  }

  private async loadNamespaces(): Promise<void> {
    try {
      const v = await this.api.invoke(version, {});
      // The route's namespace wins — it is what the user was looking at.
      this.namespace.set(this.ns() || v.namespace || '');
      this.namespaceFixed.set(!v.all_namespaces);
      if (v.all_namespaces) {
        // Namespaces that already hold clusters make useful suggestions; the
        // field stays free-form because a new one is perfectly valid.
        const all = await this.api.invoke(listAllClusters, {});
        const seen = [...new Set(all.map((c) => c.namespace).filter(Boolean))];
        this.knownNamespaces.set(seen.sort());
      }
    } catch (err) {
      this.error.set(problemDetail(err));
    }
  }

  protected readonly busy = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly crumbs = computed(() => [
    { label: 'Clusters', link: '/clusters' },
    { label: 'New cluster' },
  ]);

  protected readonly replicaChoices = REPLICA_CHOICES;
  protected readonly name = signal('');
  protected readonly replicas = signal<number>(2);
  protected readonly storage = signal('1Gi');
  // Empty means "not specified": the request omits the field and the server
  // supplies the image this deployment was configured with.
  protected readonly image = signal('');

  protected readonly advanced = signal(false);
  protected readonly credentialsSecret = signal('');
  protected readonly initConfigMap = signal('');
  protected readonly initSecret = signal('');
  protected readonly tlsSecret = signal('');
  protected readonly poolerEnabled = signal(false);
  protected readonly rows = signal<EnvRow[]>([]);

  protected readonly nameValid = computed(() => NAME_RE.test(this.name()));
  /** Why submit is unavailable, most-likely cause first. */
  protected readonly blockedReason = computed(() => {
    if (!this.namespace().trim()) {
      return 'clusters.blockedNamespace';
    }
    if (!this.nameValid()) {
      return 'clusters.blockedName';
    }
    if (!this.storageValid()) {
      return 'clusters.blockedStorage';
    }
    return 'clusters.blockedEnv';
  });
  protected readonly storageValid = computed(() => STORAGE_RE.test(this.storage()));
  protected readonly valid = computed(
    () =>
      !!this.namespace().trim() &&
      this.nameValid() &&
      this.storageValid() &&
      rowsValid(this.rows()),
  );

  cancel(): void {
    void this.router.navigate(['/clusters']);
  }

  async submit(): Promise<void> {
    const trimmed = (s: string) => (s.trim() ? s.trim() : undefined);
    const spec: ClusterSpec = {
      name: this.name(),
      replicas: this.replicas(),
      storage_size: this.storage(),
      ...(this.image().trim() ? { image: this.image().trim() } : {}),
      credentials_secret: trimmed(this.credentialsSecret()),
      env: rowsToSpec(this.rows()),
      init: {
        scripts_config_map: trimmed(this.initConfigMap()),
        scripts_secret: trimmed(this.initSecret()),
      },
      tls: { server_secret: trimmed(this.tlsSecret()) },
      ...(this.poolerEnabled() ? { pooler: { enabled: true } } : {}),
    };

    this.busy.set(true);
    this.error.set(null);
    try {
      const ns = this.namespace().trim();
      await this.api.invoke(createCluster, { ns, body: spec });
      // Land on the thing that was just made, not back on the list.
      await this.router.navigate(['/namespaces', ns, 'clusters', spec.name]);
    } catch (err) {
      this.error.set(problemDetail(err));
    } finally {
      this.busy.set(false);
    }
  }
}
