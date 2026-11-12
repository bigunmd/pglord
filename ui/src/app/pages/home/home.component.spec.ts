import { TestBed } from '@angular/core/testing';
import { Api } from '../../api/api';
import { listAllClusters } from '../../api/fn/clusters/list-all-clusters';
import { ClusterSummary } from '../../api/models/cluster-summary';
import { HomeComponent } from './home.component';

class TestHome extends HomeComponent {
  readonly summary = this.totals;
  readonly attention = this.attentionClusters;
  readonly failure = this.clusterError;
  readonly server = this.status;
  reload() {
    return this.refresh();
  }
}

const cluster = (name: string, ready: number, hibernated = false): ClusterSummary => ({
  name,
  namespace: 'production',
  image: 'postgres:18',
  replicas: 2,
  ready_replicas: ready,
  hibernated,
});

describe('overview facts', () => {
  const invoke = vi.fn();
  let component: TestHome;
  beforeEach(async () => {
    invoke
      .mockReset()
      .mockImplementation((fn) =>
        Promise.resolve(
          fn === listAllClusters
            ? [cluster('ready', 2), cluster('booting', 1), cluster('sleeping', 0, true)]
            : { version: '0.2.1' },
        ),
      );
    TestBed.configureTestingModule({ providers: [{ provide: Api, useValue: { invoke } }] });
    component = TestBed.runInInjectionContext(() => new TestHome());
    await component.reload();
  });
  afterEach(() => component.ngOnDestroy());

  it('counts actual readiness separately from intentional hibernation', () => {
    expect(component.summary()).toEqual({ total: 3, ready: 1, attention: 1, hibernated: 1 });
    expect(component.attention().map((c) => c.name)).toEqual(['booting']);
  });

  it('keeps server reachability independent from a failed cluster request', async () => {
    invoke.mockImplementation((fn) =>
      fn === listAllClusters
        ? Promise.reject({ error: { detail: 'Forbidden' } })
        : Promise.resolve({ version: '0.2.1' }),
    );
    await component.reload();
    expect(component.server()).toBe('reachable');
    expect(component.failure()).toBe('Forbidden');
  });
});
