import { Injectable, computed, inject, signal } from '@angular/core';

import { Api } from '../../api/api';
import { authConfig } from '../../api/fn/system/auth-config';
import { AuthConfigResponse } from '../../api/models/auth-config-response';

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
}

/** Holds the current API credential; the interceptor and guard read it. */
@Injectable({ providedIn: 'root' })
export class AuthService {
  private readonly api = inject(Api);
  private readonly credential = signal<StoredCredential | null>(readStored());
  private config?: Promise<AuthConfigResponse>;

  readonly current = computed(() => this.credential());
  readonly token = computed(() => this.credential()?.accessToken ?? null);

  /** Which login modes the server offers; fetched once, anonymous by design. */
  authConfig(): Promise<AuthConfigResponse> {
    this.config ??= this.api.invoke(authConfig);
    return this.config;
  }

  async enabled(): Promise<boolean> {
    const cfg = await this.authConfig();
    return cfg.static_tokens_enabled || !!cfg.oidc;
  }

  setStatic(token: string): void {
    this.store({ kind: 'static', accessToken: token });
  }

  setOidc(credential: Omit<StoredCredential, 'kind'>): void {
    this.store({ kind: 'oidc', ...credential });
  }

  logout(): void {
    this.credential.set(null);
    localStorage.removeItem(STORAGE_KEY);
  }

  private store(credential: StoredCredential): void {
    this.credential.set(credential);
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
