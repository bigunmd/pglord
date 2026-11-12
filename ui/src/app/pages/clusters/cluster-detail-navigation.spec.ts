import { TestBed } from '@angular/core/testing';
import { provideRouter, Router, withComponentInputBinding } from '@angular/router';
import { RouterTestingHarness } from '@angular/router/testing';
import { TranslocoTestingModule } from '@jsverse/transloco';
import { HlmDialogService } from '@spartan-ng/helm/dialog';
import { of } from 'rxjs';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { Api } from '../../api/api';
import { getCluster } from '../../api/fn/clusters/get-cluster';
import { listBackups } from '../../api/fn/backups/list-backups';
import { restoreCluster } from '../../api/fn/backups/restore-cluster';
import { ClusterDetail } from '../../api/models/cluster-detail';
import {
  ClusterDetailComponent,
  FenceDialogComponent,
  HibernateDialogComponent,
} from './cluster-detail.component';

const path = '/namespaces/team-a/clusters/orders';
const sections = ['overview', 'backups', 'configuration', 'access', 'maintenance'];

function cluster(): ClusterDetail {
  return {
    name: 'orders',
    namespace: 'team-a',
    image: 'postgres:18',
    storage_size: '10Gi',
    replicas: 2,
    ready_replicas: 2,
    primary: 'orders-0',
    hibernated: false,
    instances: [{ pod: 'orders-0', ready: true, role: 'primary', pending_restart: false }],
    fenced: [],
    backup: {},
    env: [],
    init: {},
    parameters: {},
    pg_hba: [],
    roles: [],
    databases: [],
    extensions: [],
    workload: {},
    tls: {},
    pooler: { spec: { enabled: false }, ready_replicas: 0, service: 'orders-pooler' },
    synchronous: false,
    sync_degraded: false,
    pending_restart: false,
  };
}

describe('cluster workspace navigation', () => {
  let data: ClusterDetail;
  let invoke: ReturnType<typeof vi.fn>;
  let dialog: { open: ReturnType<typeof vi.fn> };

  beforeEach(() => {
    data = cluster();
    invoke = vi.fn(async (operation: unknown) => {
      if (operation === getCluster) return data;
      if (operation === listBackups) return { enabled: false, backups: [], runs: [] };
      return { satisfied: true, resources: [] };
    });
    dialog = { open: vi.fn(() => ({ closed$: of(false) })) };
    TestBed.configureTestingModule({
      imports: [
        ClusterDetailComponent,
        TranslocoTestingModule.forRoot({
          langs: { en: {} },
          translocoConfig: { availableLangs: ['en'], defaultLang: 'en' },
        }),
      ],
      providers: [
        provideRouter(
          [{ path: 'namespaces/:ns/clusters/:name', component: ClusterDetailComponent }],
          withComponentInputBinding(),
        ),
        { provide: Api, useValue: { invoke } },
        { provide: HlmDialogService, useValue: dialog },
      ],
    });
  });

  async function open(query = '') {
    const harness = await RouterTestingHarness.create(path + query);
    await harness.fixture.whenStable();
    harness.detectChanges();
    return harness;
  }

  function element(harness: RouterTestingHarness, id: string) {
    return harness.routeNativeElement!.querySelector(`[data-testid="${id}"]`);
  }

  it('opens the restored cluster in the source namespace after successful confirmation', async () => {
    const harness = await open('?section=backups');
    dialog.open.mockReturnValue({ closed$: of({ target_cluster: 'orders-restored' }) });
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);
    await (harness.routeDebugElement!.componentInstance as ClusterDetailComponent).openRestore(
      data,
    );
    expect(invoke).toHaveBeenCalledWith(restoreCluster, {
      ns: 'team-a',
      name: 'orders',
      body: { target_cluster: 'orders-restored' },
    });
    expect(navigate).toHaveBeenCalledWith(['/namespaces', 'team-a', 'clusters', 'orders-restored']);
  });

  it('defaults to a health-focused overview and keeps other tasks out of it', async () => {
    const harness = await open();
    expect(element(harness, 'workspace-overview')).not.toBeNull();
    expect(element(harness, 'instance-orders-0')).not.toBeNull();
    expect(element(harness, 'open-config')).toBeNull();
    expect(element(harness, 'open-upgrade')).toBeNull();
    expect(element(harness, 'section-overview')?.getAttribute('aria-current')).toBe('page');
    expect(invoke).toHaveBeenCalledWith(getCluster, { ns: 'team-a', name: 'orders' });
  });

  it('opens deep links, switches with native links and preserves namespace and query context', async () => {
    const harness = await open('?section=backups&context=operator');
    expect(element(harness, 'workspace-backups')).not.toBeNull();
    const link = element(harness, 'section-configuration') as HTMLAnchorElement;
    expect(link.tagName).toBe('A');
    expect(link.getAttribute('href')).toContain(path);
    expect(link.getAttribute('href')).toContain('context=operator');
    link.click();
    await harness.fixture.whenStable();
    harness.detectChanges();
    expect(TestBed.inject(Router).url).toContain('section=configuration');
    expect(element(harness, 'workspace-configuration')).not.toBeNull();
    expect(element(harness, 'workspace-backups')).toBeNull();
    expect(element(harness, 'section-configuration')?.getAttribute('aria-current')).toBe('page');
    expect(element(harness, 'section-backups')?.hasAttribute('aria-current')).toBe(false);
  });

  it('falls back to overview for an unknown deep-link section', async () => {
    const harness = await open('?section=not-a-section');
    expect(element(harness, 'workspace-overview')).not.toBeNull();
    expect(element(harness, 'section-overview')?.getAttribute('aria-current')).toBe('page');
  });

  it.each(sections)('keeps operational warnings visible in %s', async (section) => {
    data = {
      ...data,
      hibernated: true,
      pending_restart: true,
      sync_degraded: true,
      restore: { source_cluster: 'source', repository: {} },
      upgrade: {
        mode: 'blue_green',
        phase: 'migrating',
        ready_to_cutover: false,
        target_image: 'postgres:19',
        next_generation: 'next',
        databases: [],
        error: 'Replication blocked',
      },
    };
    const harness = await open(`?section=${section}`);
    for (const id of [
      'hibernated-banner',
      'restored-from',
      'restart-banner',
      'sync-degraded-banner',
      'upgrade-banner',
    ]) {
      expect(element(harness, id), id).not.toBeNull();
    }
    expect((element(harness, 'open-restart') as HTMLButtonElement).disabled).toBe(true);
    expect(harness.routeNativeElement!.textContent).toContain('Replication blocked');
  });

  it.each([
    ['backups', 'open-backup-config'],
    ['configuration', 'open-config'],
    ['access', 'open-hba'],
    ['maintenance', 'open-upgrade'],
  ])('keeps the %s task action reachable', async (section, action) => {
    const harness = await open(`?section=${section}`);
    expect(element(harness, action)).not.toBeNull();
  });

  it('keeps primary fencing reachable and requires confirmation before mutation', async () => {
    const harness = await open();
    (element(harness, 'fence-orders-0') as HTMLButtonElement).click();
    await harness.fixture.whenStable();
    expect(dialog.open).toHaveBeenCalledWith(FenceDialogComponent, {
      context: { pod: 'orders-0' },
    });
    expect(invoke).toHaveBeenCalledTimes(3);
  });

  it('keeps hibernation confirmation and performs no mutation on cancel', async () => {
    const harness = await open('?section=maintenance');
    (element(harness, 'open-hibernate') as HTMLButtonElement).click();
    await harness.fixture.whenStable();
    expect(dialog.open).toHaveBeenCalledWith(HibernateDialogComponent, {
      context: { name: 'orders' },
    });
    expect(invoke).toHaveBeenCalledTimes(3);
  });
});
