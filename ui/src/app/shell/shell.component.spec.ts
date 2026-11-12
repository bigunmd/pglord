import { Component } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter, Router } from '@angular/router';
import { RouterTestingHarness } from '@angular/router/testing';
import { TranslocoTestingModule } from '@jsverse/transloco';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ShellComponent } from './shell.component';
import { AuthService } from '../core/auth/auth.service';
import { OidcService } from '../core/auth/oidc.service';
import { ThemeService } from '../core/theme/theme.service';

@Component({ template: '<h1>Cluster</h1>' })
class TestPage {}

describe('workspace shell', () => {
  beforeEach(() => {
    TestBed.configureTestingModule({
      imports: [
        ShellComponent,
        TranslocoTestingModule.forRoot({
          langs: { en: {} },
          translocoConfig: { availableLangs: ['en'], defaultLang: 'en' },
        }),
      ],
      providers: [
        provideRouter([
          {
            path: '',
            component: ShellComponent,
            children: [
              { path: 'namespaces/:ns/clusters/:name', component: TestPage },
              { path: '', component: TestPage },
            ],
          },
        ]),
        {
          provide: ThemeService,
          useValue: { resolved: () => 'light', mode: () => 'system', setMode: vi.fn() },
        },
        { provide: AuthService, useValue: { current: () => null } },
        { provide: OidcService, useValue: { logout: vi.fn() } },
      ],
    });
  });

  it('marks clusters active on namespace-qualified detail routes and skips without leaving the route', async () => {
    const path = '/namespaces/team-a/clusters/orders?section=backups';
    const harness = await RouterTestingHarness.create(path);
    const root = harness.routeNativeElement!;
    const link = root.querySelector('.pg-nav-link[aria-current="page"]')!;
    expect(link.getAttribute('href')).toBe('/clusters');
    const main = root.querySelector('main') as HTMLElement;
    const focus = vi.spyOn(main, 'focus');
    main.scrollIntoView = vi.fn();
    const event = new MouseEvent('click', { bubbles: true, cancelable: true });
    root.querySelector('.pg-skip-link')!.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(focus).toHaveBeenCalled();
    expect(main.scrollIntoView).toHaveBeenCalled();
    expect(TestBed.inject(Router).url).toBe(path);
  });
});
