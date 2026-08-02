import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';

import { AuthService, STORAGE_KEY } from './auth.service';
import { installMemoryLocalStorage } from './testing/memory-storage';

describe('AuthService', () => {
  beforeAll(() => installMemoryLocalStorage());

  beforeEach(() => {
    localStorage.clear();
    TestBed.configureTestingModule({
      providers: [provideHttpClient(), provideHttpClientTesting()],
    });
  });

  it('persists a static token to localStorage', () => {
    const service = TestBed.inject(AuthService);
    service.setStatic('tok-a');
    expect(service.token()).toBe('tok-a');
    const stored = JSON.parse(localStorage.getItem(STORAGE_KEY)!);
    expect(stored.kind).toBe('static');
    expect(stored.accessToken).toBe('tok-a');
  });

  it('restores a stored credential on startup', () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ kind: 'static', accessToken: 'tok-b' }));
    const service = TestBed.inject(AuthService);
    expect(service.token()).toBe('tok-b');
  });

  it('ignores corrupt stored credentials', () => {
    localStorage.setItem(STORAGE_KEY, 'not json');
    const service = TestBed.inject(AuthService);
    expect(service.token()).toBeNull();
  });

  it('logout clears the credential and storage', () => {
    const service = TestBed.inject(AuthService);
    service.setStatic('tok-a');
    service.logout();
    expect(service.token()).toBeNull();
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });
});
