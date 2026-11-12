import { TestBed } from '@angular/core/testing';
import { provideRouter, Router, withComponentInputBinding } from '@angular/router';
import { RouterTestingHarness } from '@angular/router/testing';
import { TranslocoTestingModule } from '@jsverse/transloco';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Api } from '../../api/api';
import { createCluster } from '../../api/fn/clusters/create-cluster';
import { listAllClusters } from '../../api/fn/clusters/list-all-clusters';
import { ClusterCreatePageComponent } from './cluster-create-page.component';

describe('cluster creation workspace', () => {
  let invoke: ReturnType<typeof vi.fn>;
  beforeEach(() => {
    invoke = vi.fn(async (operation: unknown) =>
      operation === listAllClusters ? [] : { namespace: 'team-a', default_image: 'postgres:18' },
    );
    TestBed.configureTestingModule({
      imports: [
        ClusterCreatePageComponent,
        TranslocoTestingModule.forRoot({
          langs: { en: {} },
          translocoConfig: { availableLangs: ['en'], defaultLang: 'en' },
        }),
      ],
      providers: [
        provideRouter(
          [{ path: 'clusters/new', component: ClusterCreatePageComponent }],
          withComponentInputBinding(),
        ),
        { provide: Api, useValue: { invoke } },
      ],
    });
  });

  async function open() {
    const harness = await RouterTestingHarness.create('/clusters/new');
    await harness.fixture.whenStable();
    harness.detectChanges();
    return harness;
  }

  it('keeps advanced controls collapsed and exposes a labeled replica button group', async () => {
    const harness = await open();
    const root = harness.routeNativeElement!;
    const toggle = root.querySelector('[data-testid="toggle-advanced"]') as HTMLButtonElement;
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(root.querySelector('#pg-cluster-image')).toBeNull();
    const group = root.querySelector('[role="group"]')!;
    expect(group.getAttribute('aria-labelledby')).toBe('replicas-label');
    expect(group.querySelectorAll('[aria-pressed="true"]')).toHaveLength(1);
    toggle.click();
    harness.detectChanges();
    expect(root.querySelector('#pg-cluster-image')).not.toBeNull();
  });

  it('explains invalid submission and creates in the server namespace after entering a valid name', async () => {
    const harness = await open();
    const root = harness.routeNativeElement!;
    const submit = root.querySelector('[data-testid="cluster-submit"]') as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    expect(root.querySelector('[data-testid="cluster-submit-blocked"]')).not.toBeNull();
    const input = root.querySelector('#pg-cluster-name') as HTMLInputElement;
    input.value = 'orders';
    input.dispatchEvent(new Event('input'));
    await harness.fixture.whenStable();
    harness.detectChanges();
    expect(submit.disabled).toBe(false);
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);
    submit.click();
    await harness.fixture.whenStable();
    expect(invoke).toHaveBeenCalledWith(
      createCluster,
      expect.objectContaining({ ns: 'team-a', body: expect.objectContaining({ name: 'orders' }) }),
    );
    expect(navigate).toHaveBeenCalledWith(['/namespaces', 'team-a', 'clusters', 'orders']);
  });
});
