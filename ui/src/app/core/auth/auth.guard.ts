import { inject } from '@angular/core';
import { CanActivateFn, Router } from '@angular/router';

import { AuthService } from './auth.service';
import { OidcService } from './oidc.service';

/** Blocks the shell until a credential exists — but only when the server
 * actually has auth enabled, so an unauthenticated install never sees a
 * login screen. */
export const authGuard: CanActivateFn = async () => {
  const auth = inject(AuthService);
  const oidc = inject(OidcService);
  const router = inject(Router);
  try {
    if (!(await auth.enabled())) {
      return true;
    }
  } catch {
    // Config unreachable (server down?): let the page load and surface the
    // real API errors instead of bouncing to a login screen that cannot work.
    return true;
  }
  if (auth.token()) {
    oidc.ensureRefreshScheduled();
    return true;
  }
  return router.createUrlTree(['/login']);
};
