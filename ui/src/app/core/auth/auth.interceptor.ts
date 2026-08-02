import { HttpErrorResponse, HttpInterceptorFn } from '@angular/common/http';
import { inject } from '@angular/core';
import { Router } from '@angular/router';
import { catchError, throwError } from 'rxjs';

import { ApiConfiguration } from '../../api/api-configuration';
import { authConfig } from '../../api/fn/system/auth-config';
import { AuthService } from './auth.service';

/** Attaches the bearer credential to API calls; a 401 answer means the
 * credential is gone or stale, so drop it and return to the login screen. */
export const authInterceptor: HttpInterceptorFn = (req, next) => {
  const auth = inject(AuthService);
  const router = inject(Router);
  const rootUrl = inject(ApiConfiguration).rootUrl;

  const isApi =
    req.url.startsWith('/api/') || (rootUrl !== '' && req.url.startsWith(`${rootUrl}/api/`));
  const token = auth.token();
  const outgoing =
    isApi && token ? req.clone({ setHeaders: { Authorization: `Bearer ${token}` } }) : req;

  return next(outgoing).pipe(
    catchError((err: unknown) => {
      if (
        isApi &&
        err instanceof HttpErrorResponse &&
        err.status === 401 &&
        !req.url.endsWith(authConfig.PATH)
      ) {
        auth.logout();
        void router.navigate(['/login']);
      }
      return throwError(() => err);
    }),
  );
};
