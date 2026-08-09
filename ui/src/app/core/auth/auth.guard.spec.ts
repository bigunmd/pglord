import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { Router, UrlTree } from '@angular/router';
import { provideRouter } from '@angular/router';

import { AuthConfigResponse } from '../../api/models/auth-config-response';
import { authGuard } from './auth.guard';
import { AuthService } from './auth.service';
import { installMemoryLocalStorage } from './testing/memory-storage';

/** Runs the guard inside an injection context, as the router would. */
function runGuard(): Promise<boolean | UrlTree> {
  return TestBed.runInInjectionContext(
    () => authGuard(null!, null!) as Promise<boolean | UrlTree>,
  );
}

function stubConfig(auth: AuthService, cfg: Partial<AuthConfigResponse>): void {
  vi.spyOn(auth, 'authConfig').mockResolvedValue({
    static_tokens_enabled: false,
    oidc: null,
    kubernetes_enabled: false,
    scopes: 'openid profile email offline_access',
    ...cfg,
  } as AuthConfigResponse);
}

describe('authGuard', () => {
  beforeAll(() => installMemoryLocalStorage());

  beforeEach(() => {
    localStorage.clear();
    TestBed.configureTestingModule({
      providers: [provideHttpClient(), provideHttpClientTesting(), provideRouter([])],
    });
  });

  it('sends a Kubernetes-only install to the login screen', async () => {
    // Regression: enabled() ignored kubernetes_enabled, so a k8s-only server
    // let the shell load without a credential and the user collected 401s
    // instead of ever being offered the token field.
    const auth = TestBed.inject(AuthService);
    stubConfig(auth, { kubernetes_enabled: true });

    const result = await runGuard();

    expect(result).not.toBe(true);
    expect(TestBed.inject(Router).serializeUrl(result as UrlTree)).toBe('/login');
  });

  it('lets the shell load when the server has no auth configured', async () => {
    const auth = TestBed.inject(AuthService);
    stubConfig(auth, {});

    await expect(runGuard()).resolves.toBe(true);
  });

  it('lets the shell load when the auth config cannot be reached', async () => {
    // A login screen cannot work if the server is down, so bouncing there
    // hides the real error instead of showing it.
    const auth = TestBed.inject(AuthService);
    vi.spyOn(auth, 'authConfig').mockRejectedValue(new Error('offline'));

    await expect(runGuard()).resolves.toBe(true);
  });

  it('admits a request that already carries a credential', async () => {
    const auth = TestBed.inject(AuthService);
    stubConfig(auth, { static_tokens_enabled: true });
    auth.setStatic('tok-a');

    await expect(runGuard()).resolves.toBe(true);
  });
});
