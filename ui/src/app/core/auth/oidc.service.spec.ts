import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { Router, provideRouter } from '@angular/router';

import { AuthService } from './auth.service';
import { OidcService } from './oidc.service';
import { installMemoryLocalStorage } from './testing/memory-storage';

const ISSUER = 'https://idp.example.com';
const DISCOVERY = {
  authorization_endpoint: `${ISSUER}/authorize`,
  token_endpoint: `${ISSUER}/token`,
  end_session_endpoint: `${ISSUER}/logout`,
};

/** Captures location.assign so the redirect can be asserted, not performed. */
function captureRedirects(): string[] {
  const seen: string[] = [];
  Object.defineProperty(globalThis, 'location', {
    value: { origin: 'https://pglord.example.com', assign: (url: string) => void seen.push(url) },
    configurable: true,
  });
  return seen;
}

/** Queues JSON responses for the service's bare `fetch` calls. */
function stubFetch(...responses: unknown[]): ReturnType<typeof vi.fn> {
  const fetchMock = vi.fn();
  for (const body of responses) {
    fetchMock.mockResolvedValueOnce({ ok: true, json: async () => body });
  }
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

function bodyOf(fetchMock: ReturnType<typeof vi.fn>, call: number): URLSearchParams {
  return new URLSearchParams(fetchMock.mock.calls[call][1].body as string);
}

/** Fire the refresh timer without waiting out its 5s floor, then let the
 * promises it starts settle. Timers must already be faked when the service
 * schedules, so callers install them before logging in. */
async function runPendingTimers(): Promise<void> {
  await vi.advanceTimersByTimeAsync(10_000);
}

describe('OidcService', () => {
  let redirects: string[];

  beforeAll(() => installMemoryLocalStorage());

  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    redirects = captureRedirects();
    TestBed.configureTestingModule({
      providers: [provideHttpClient(), provideHttpClientTesting(), provideRouter([])],
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('redirects with a PKCE S256 challenge and the scopes it was given', async () => {
    stubFetch(DISCOVERY);
    await TestBed.inject(OidcService).beginLogin(ISSUER, 'pglord', 'openid email offline_access');

    const url = new URL(redirects[0]);
    expect(url.origin + url.pathname).toBe(`${ISSUER}/authorize`);
    expect(url.searchParams.get('response_type')).toBe('code');
    expect(url.searchParams.get('code_challenge_method')).toBe('S256');
    // Scopes must come from the server: hardcoding them is what dropped
    // offline_access and left the refresh timer with nothing to refresh.
    expect(url.searchParams.get('scope')).toBe('openid email offline_access');
    expect(url.searchParams.get('redirect_uri')).toBe(
      'https://pglord.example.com/login/callback',
    );

    // The challenge is the SHA-256 of the stored verifier, not the verifier.
    const pending = JSON.parse(sessionStorage.getItem('pglord.oidc')!);
    const challenge = url.searchParams.get('code_challenge')!;
    expect(challenge).not.toBe(pending.verifier);
    expect(challenge).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(url.searchParams.get('state')).toBe(pending.state);
  });

  it('aborts the callback when the state does not match', async () => {
    stubFetch(DISCOVERY);
    const oidc = TestBed.inject(OidcService);
    await oidc.beginLogin(ISSUER, 'pglord', 'openid');

    await expect(oidc.completeLogin('the-code', 'not-the-state')).rejects.toThrow(/CSRF/);
    expect(TestBed.inject(AuthService).token()).toBeNull();
  });

  it('refuses a callback that no login started', async () => {
    await expect(TestBed.inject(OidcService).completeLogin('c', 's')).rejects.toThrow(
      /No login in progress/,
    );
  });

  it('exchanges the code with the verifier and stores the credential', async () => {
    const fetchMock = stubFetch(DISCOVERY, {
      access_token: 'at-1',
      refresh_token: 'rt-1',
      expires_in: 3600,
    });
    const oidc = TestBed.inject(OidcService);
    await oidc.beginLogin(ISSUER, 'pglord', 'openid');
    const pending = JSON.parse(sessionStorage.getItem('pglord.oidc')!);

    await oidc.completeLogin('the-code', pending.state);

    const form = bodyOf(fetchMock, 1);
    expect(form.get('grant_type')).toBe('authorization_code');
    expect(form.get('code')).toBe('the-code');
    expect(form.get('code_verifier')).toBe(pending.verifier);

    const auth = TestBed.inject(AuthService);
    expect(auth.token()).toBe('at-1');
    // Consumed on read, so a replayed callback cannot reuse the verifier.
    expect(sessionStorage.getItem('pglord.oidc')).toBeNull();
    expect(auth.current()?.endSessionEndpoint).toBe(`${ISSUER}/logout`);
  });

  it('keeps the old refresh token when the IdP does not rotate it', async () => {
    vi.useFakeTimers();
    const fetchMock = stubFetch(
      DISCOVERY,
      { access_token: 'at-1', refresh_token: 'rt-1', expires_in: 1 },
      { access_token: 'at-2', expires_in: 3600 },
    );
    const oidc = TestBed.inject(OidcService);
    const auth = TestBed.inject(AuthService);
    await oidc.beginLogin(ISSUER, 'pglord', 'openid');
    await oidc.completeLogin('c', JSON.parse(sessionStorage.getItem('pglord.oidc')!).state);

    // A near-instant expiry still schedules against the 5s floor.
    await runPendingTimers();

    expect(auth.token()).toBe('at-2');
    expect(bodyOf(fetchMock, 2).get('grant_type')).toBe('refresh_token');
    expect(auth.current()?.refreshToken).toBe('rt-1');
  });

  it('logs out and returns to the login screen when a refresh fails', async () => {
    vi.useFakeTimers();
    const fetchMock = stubFetch(DISCOVERY, {
      access_token: 'at-1',
      refresh_token: 'rt-1',
      expires_in: 1,
    });
    fetchMock.mockResolvedValueOnce({ ok: false, status: 400 });
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);
    const oidc = TestBed.inject(OidcService);
    const auth = TestBed.inject(AuthService);
    await oidc.beginLogin(ISSUER, 'pglord', 'openid');
    await oidc.completeLogin('c', JSON.parse(sessionStorage.getItem('pglord.oidc')!).state);

    await runPendingTimers();

    expect(auth.token()).toBeNull();
    expect(navigate).toHaveBeenCalledWith(['/login']);
  });

  it('ends the IdP session on logout when the issuer published an endpoint', async () => {
    stubFetch(DISCOVERY, { access_token: 'at-1', expires_in: 3600 });
    const oidc = TestBed.inject(OidcService);
    await oidc.beginLogin(ISSUER, 'pglord', 'openid');
    await oidc.completeLogin('c', JSON.parse(sessionStorage.getItem('pglord.oidc')!).state);

    oidc.logout();

    expect(TestBed.inject(AuthService).token()).toBeNull();
    const url = new URL(redirects[1]);
    expect(url.origin + url.pathname).toBe(`${ISSUER}/logout`);
    expect(url.searchParams.get('client_id')).toBe('pglord');
    expect(url.searchParams.get('post_logout_redirect_uri')).toBe(
      'https://pglord.example.com/login',
    );
  });

  it('falls back to a local logout when there is no end_session_endpoint', async () => {
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);
    const auth = TestBed.inject(AuthService);
    auth.setStatic('tok-a');

    TestBed.inject(OidcService).logout();

    expect(auth.token()).toBeNull();
    expect(navigate).toHaveBeenCalledWith(['/login']);
    expect(redirects).toHaveLength(0);
  });
});
