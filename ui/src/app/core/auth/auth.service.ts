import { Injectable, computed, inject, signal } from '@angular/core';

import { Api } from '../../api/api';
import { authConfig } from '../../api/fn/system/auth-config';
import { whoami } from '../../api/fn/system/whoami';
import { AuthConfigResponse } from '../../api/models/auth-config-response';
import { WhoamiResponse } from '../../api/models/whoami-response';

export const STORAGE_KEY = 'pglord.auth';

export interface StoredCredential {
  kind: 'static' | 'oidc';
  accessToken: string;
  refreshToken?: string;
  /** Epoch milliseconds when the access token expires. */
  expiresAt?: number;
  /** OIDC only: kept so a refresh can run after a page reload. */
  tokenEndpoint?: string;
  clientId?: string;
  /** OIDC only: discovered at login so logout can end the IdP session too. */
  endSessionEndpoint?: string;
}

/** Holds the current API credential; the interceptor and guard read it. */
@Injectable({ providedIn: 'root' })
export class AuthService {
  private readonly api = inject(Api);
  private readonly credential = signal<StoredCredential | null>(readStored());
  private config?: Promise<AuthConfigResponse>;
  private identity?: Promise<WhoamiResponse>;

  readonly current = computed(() => this.credential());
  readonly token = computed(() => this.credential()?.accessToken ?? null);

  /** Which login modes the server offers; fetched once, anonymous by design. */
  authConfig(): Promise<AuthConfigResponse> {
    this.config ??= this.api.invoke(authConfig);
    return this.config;
  }

  /** Who the server resolved this credential to; fetched once per session. */
  whoami(): Promise<WhoamiResponse> {
    this.identity ??= this.api.invoke(whoami);
    return this.identity;
  }

  async enabled(): Promise<boolean> {
    const cfg = await this.authConfig();
    // Kubernetes tokens count: without them a k8s-only install never routes
    // to /login and the user just collects 401s.
    return cfg.static_tokens_enabled || !!cfg.oidc || cfg.kubernetes_enabled;
  }

  setStatic(token: string): void {
    this.store({ kind: 'static', accessToken: token });
  }

  setOidc(credential: Omit<StoredCredential, 'kind'>): void {
    this.store({ kind: 'oidc', ...credential });
  }

  logout(): void {
    this.credential.set(null);
    this.identity = undefined;
    localStorage.removeItem(STORAGE_KEY);
  }

  private store(credential: StoredCredential): void {
    this.credential.set(credential);
    this.identity = undefined;
    localStorage.setItem(STORAGE_KEY, JSON.stringify(credential));
  }
}

function readStored(): StoredCredential | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw ? (JSON.parse(raw) as StoredCredential) : null;
  } catch {
    return null;
  }
}
