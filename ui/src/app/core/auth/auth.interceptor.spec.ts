import { HttpClient, provideHttpClient, withInterceptors } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';

import { authInterceptor } from './auth.interceptor';
import { AuthService } from './auth.service';
import { installMemoryLocalStorage } from './testing/memory-storage';

describe('authInterceptor', () => {
  beforeAll(() => installMemoryLocalStorage());

  beforeEach(() => {
    localStorage.clear();
    TestBed.configureTestingModule({
      providers: [
        provideRouter([{ path: 'login', children: [] }]),
        provideHttpClient(withInterceptors([authInterceptor])),
        provideHttpClientTesting(),
      ],
    });
  });

  afterEach(() => TestBed.inject(HttpTestingController).verify());

  it('attaches the bearer header to API requests', () => {
    TestBed.inject(AuthService).setStatic('tok-a');
    TestBed.inject(HttpClient).get('/api/v1/clusters').subscribe();
    const req = TestBed.inject(HttpTestingController).expectOne('/api/v1/clusters');
    expect(req.request.headers.get('Authorization')).toBe('Bearer tok-a');
    req.flush([]);
  });

  it('leaves non-API requests untouched', () => {
    TestBed.inject(AuthService).setStatic('tok-a');
    TestBed.inject(HttpClient).get('/i18n/en.json').subscribe();
    const req = TestBed.inject(HttpTestingController).expectOne('/i18n/en.json');
    expect(req.request.headers.has('Authorization')).toBe(false);
    req.flush({});
  });

  it('sends no header when logged out', () => {
    TestBed.inject(HttpClient).get('/api/v1/clusters').subscribe({ error: () => undefined });
    const req = TestBed.inject(HttpTestingController).expectOne('/api/v1/clusters');
    expect(req.request.headers.has('Authorization')).toBe(false);
    req.flush([]);
  });

  it('a 401 clears the credential', () => {
    const auth = TestBed.inject(AuthService);
    auth.setStatic('expired');
    TestBed.inject(HttpClient)
      .get('/api/v1/clusters')
      .subscribe({ error: () => undefined });
    const req = TestBed.inject(HttpTestingController).expectOne('/api/v1/clusters');
    req.flush({ code: 'auth_invalid' }, { status: 401, statusText: 'Unauthorized' });
    expect(auth.token()).toBeNull();
  });
});
