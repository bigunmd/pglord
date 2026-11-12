import { TestBed } from '@angular/core/testing';
import { Router } from '@angular/router';
import { HlmDialogService } from '@spartan-ng/helm/dialog';
import { of } from 'rxjs';
import { Api } from '../../api/api';
import { deleteCluster } from '../../api/fn/clusters/delete-cluster';
import { ClusterSummary } from '../../api/models/cluster-summary';
import { installMemoryLocalStorage } from '../../core/auth/testing/memory-storage';
import { ClustersComponent } from './clusters.component';

class TestClusters extends ClustersComponent {
  readonly items = this.visible;
  readonly options = this.namespaces;
  readonly searchQuery = this.query;
  readonly namespace = this.selectedNs;
  readonly failure = this.error;
  readonly ready = this.loaded;
  reload() {
    return this.refresh();
  }
  clear() {
    this.clearFilters();
  }
}

const cluster = (namespace: string, name = 'orders'): ClusterSummary => ({
  namespace,
  name,
  hibernated: false,
  image: 'postgres:18',
  replicas: 2,
  ready_replicas: 2,
});

const sources = import.meta.glob('./clusters.component.ts', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

describe('cluster directory', () => {
  let component: TestClusters;
  const invoke = vi.fn();
  const navigate = vi.fn();
  const open = vi.fn();
  beforeAll(() => installMemoryLocalStorage());
  beforeEach(async () => {
    localStorage.clear();
    vi.clearAllMocks();
    invoke.mockResolvedValue([
      cluster('production'),
      cluster('staging'),
      cluster('staging', 'analytics'),
    ]);
    TestBed.configureTestingModule({
      providers: [
        { provide: Api, useValue: { invoke } },
        { provide: Router, useValue: { navigate } },
        { provide: HlmDialogService, useValue: { open } },
      ],
    });
    component = TestBed.runInInjectionContext(() => new TestClusters());
    await component.reload();
  });
  afterEach(() => component.ngOnDestroy());

  it('combines case-insensitive name or namespace search with exact namespace scope', () => {
    component.searchQuery.set('  ORDERS ');
    expect(component.items().length).toBe(2);
    component.namespace.set('staging');
    expect(component.items().map((c) => c.namespace)).toEqual(['staging']);
    component.searchQuery.set('production');
    expect(component.items()).toEqual([]);
  });

  it('keeps a persisted namespace visible even when it has no clusters and can reset it', () => {
    component.namespace.set('removed');
    localStorage.setItem('pglord.namespace', 'removed');
    expect(component.options()).toContain('removed');
    expect(component.items()).toEqual([]);
    component.clear();
    expect(component.items().length).toBe(3);
    expect(localStorage.getItem('pglord.namespace')).toBe('');
  });

  it('preserves chosen namespace for create without guessing a default', () => {
    component.openCreate();
    expect(navigate).toHaveBeenLastCalledWith(['/clusters/new'], {});
    component.namespace.set('staging');
    component.openCreate();
    expect(navigate).toHaveBeenLastCalledWith(['/clusters/new'], {
      queryParams: { ns: 'staging' },
    });
  });

  it('deletes only after confirmation and sends the exact namespace', async () => {
    open.mockReturnValueOnce({ closed$: of(false) });
    await component.openDelete(cluster('staging'));
    expect(invoke).not.toHaveBeenCalledWith(deleteCluster, expect.anything());
    open.mockReturnValueOnce({ closed$: of(true) });
    await component.openDelete(cluster('production'));
    expect(invoke).toHaveBeenCalledWith(deleteCluster, { ns: 'production', name: 'orders' });
  });

  it('retains last-known data and exposes refresh failure', async () => {
    invoke.mockRejectedValueOnce({ error: { detail: 'Temporarily unavailable' } });
    await component.reload();
    expect(component.failure()).toBe('Temporarily unavailable');
    expect(component.items().length).toBe(3);
    expect(component.ready()).toBe(true);
  });

  it('uses namespace-qualified native links, not clickable rows', () => {
    const source = sources['./clusters.component.ts'];
    expect(source).toContain(`[routerLink]="['/namespaces', c.namespace, 'clusters', c.name]"`);
    expect(source).not.toContain('(click)="openDetail(c)"');
  });
});
