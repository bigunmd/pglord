import { Injectable, inject } from '@angular/core';
import { Router } from '@angular/router';

import { AuthService, StoredCredential } from './auth.service';

interface OidcDiscovery {
  authorization_endpoint: string;
  token_endpoint: string;
  end_session_endpoint?: string;
}

interface PendingLogin {
  verifier: string;
  state: string;
  tokenEndpoint: string;
  clientId: string;
  endSessionEndpoint?: string;
}

interface TokenResponse {
  access_token: string;
  refresh_token?: string;
  expires_in?: number;
}

const PENDING_KEY = 'pglord.oidc';
/** Refresh this long before the access token expires. */
const REFRESH_MARGIN_MS = 60_000;

/** Hand-rolled authorization-code + PKCE flow (public client, no secret).
 * Small on purpose: discovery fetch, redirect out, code exchange back,
 * refresh timer. Third-party OIDC libraries chronically lag Angular majors. */
@Injectable({ providedIn: 'root' })
export class OidcService {
  private readonly auth = inject(AuthService);
  private readonly router = inject(Router);
  private refreshTimer?: ReturnType<typeof setTimeout>;

  /** Redirect to the IdP's authorize endpoint. Never returns normally.
   * `scopes` comes from the server: hardcoding them here is what left
   * `offline_access` out and made the refresh timer dead code. */
  async beginLogin(issuer: string, clientId: string, scopes: string): Promise<void> {
    const discovery = await this.fetchDiscovery(issuer);
    const verifier = randomUrlSafe(64);
    const state = randomUrlSafe(32);
    const pending: PendingLogin = {
      verifier,
      state,
      tokenEndpoint: discovery.token_endpoint,
      clientId,
      endSessionEndpoint: discovery.end_session_endpoint,
    };
    sessionStorage.setItem(PENDING_KEY, JSON.stringify(pending));
    const params = new URLSearchParams({
      response_type: 'code',
      client_id: clientId,
      redirect_uri: redirectUri(),
      scope: scopes,
      state,
      code_challenge: await s256(verifier),
      code_challenge_method: 'S256',
    });
    location.assign(`${discovery.authorization_endpoint}?${params}`);
  }

  /** Clear the local credential, and end the IdP session too when the issuer
   * published an end_session_endpoint — otherwise "sign out" only signs you
   * out until the next silent redirect hands the session straight back. */
  logout(): void {
    const credential = this.auth.current();
    const endpoint = credential?.endSessionEndpoint;
    const clientId = credential?.clientId;
    clearTimeout(this.refreshTimer);
    this.refreshTimer = undefined;
    this.auth.logout();
    if (!endpoint) {
      void this.router.navigate(['/login']);
      return;
    }
    const params = new URLSearchParams({
      post_logout_redirect_uri: `${location.origin}/login`,
    });
    if (clientId) {
      params.set('client_id', clientId);
    }
    location.assign(`${endpoint}?${params}`);
  }

  /** Exchange the code on /login/callback; stores the credential. */
  async completeLogin(code: string, state: string): Promise<void> {
    const raw = sessionStorage.getItem(PENDING_KEY);
    if (!raw) {
      throw new Error('No login in progress');
    }
    sessionStorage.removeItem(PENDING_KEY);
    const pending = JSON.parse(raw) as PendingLogin;
    if (state !== pending.state) {
      throw new Error('State mismatch — possible CSRF, login aborted');
    }
    const tokens = await postForm(pending.tokenEndpoint, {
      grant_type: 'authorization_code',
      code,
      redirect_uri: redirectUri(),
      client_id: pending.clientId,
      code_verifier: pending.verifier,
    });
    this.storeTokens(tokens, pending.tokenEndpoint, pending.clientId, pending.endSessionEndpoint);
  }

  /** Re-arm the refresh timer from the stored credential (page reloads). */
  ensureRefreshScheduled(): void {
    const credential = this.auth.current();
    if (credential?.kind === 'oidc' && !this.refreshTimer) {
      this.schedule(credential);
    }
  }

  private storeTokens(
    tokens: TokenResponse,
    tokenEndpoint: string,
    clientId: string,
    endSessionEndpoint?: string,
  ): void {
    const credential: Omit<StoredCredential, 'kind'> = {
      accessToken: tokens.access_token,
      refreshToken: tokens.refresh_token,
      expiresAt: tokens.expires_in ? Date.now() + tokens.expires_in * 1000 : undefined,
      tokenEndpoint,
      clientId,
      endSessionEndpoint,
    };
    this.auth.setOidc(credential);
    this.schedule({ kind: 'oidc', ...credential });
  }

  private schedule(credential: StoredCredential): void {
    clearTimeout(this.refreshTimer);
    this.refreshTimer = undefined;
    const { refreshToken, expiresAt, tokenEndpoint, clientId } = credential;
    if (!refreshToken || !expiresAt || !tokenEndpoint || !clientId) {
      return;
    }
    const delay = Math.max(expiresAt - Date.now() - REFRESH_MARGIN_MS, 5_000);
    this.refreshTimer = setTimeout(() => void this.refresh(credential), delay);
  }

  private async refresh(credential: StoredCredential): Promise<void> {
    this.refreshTimer = undefined;
    try {
      const tokens = await postForm(credential.tokenEndpoint!, {
        grant_type: 'refresh_token',
        refresh_token: credential.refreshToken!,
        client_id: credential.clientId!,
      });
      // Rotating refresh tokens: keep the new one when granted.
      tokens.refresh_token ??= credential.refreshToken;
      this.storeTokens(
        tokens,
        credential.tokenEndpoint!,
        credential.clientId!,
        credential.endSessionEndpoint,
      );
    } catch {
      this.auth.logout();
      void this.router.navigate(['/login']);
    }
  }

  private async fetchDiscovery(issuer: string): Promise<OidcDiscovery> {
    const url = `${issuer.replace(/\/$/, '')}/.well-known/openid-configuration`;
    const resp = await fetch(url);
    if (!resp.ok) {
      throw new Error(`OIDC discovery failed: ${resp.status}`);
    }
    return (await resp.json()) as OidcDiscovery;
  }
}

function redirectUri(): string {
  return `${location.origin}/login/callback`;
}

async function postForm(url: string, fields: Record<string, string>): Promise<TokenResponse> {
  const resp = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams(fields).toString(),
  });
  if (!resp.ok) {
    throw new Error(`Token request failed: ${resp.status}`);
  }
  return (await resp.json()) as TokenResponse;
}

function randomUrlSafe(bytes: number): string {
  const buf = new Uint8Array(bytes);
  crypto.getRandomValues(buf);
  return base64Url(buf);
}

async function s256(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier));
  return base64Url(new Uint8Array(digest));
}

function base64Url(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
}
