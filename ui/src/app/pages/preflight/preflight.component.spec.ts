import { TestBed } from '@angular/core/testing';
import { Api } from '../../api/api';
import { getPreflight } from '../../api/fn/system/get-preflight';
import { PreflightComponent } from './preflight.component';

class TestPreflight extends PreflightComponent {
  readonly result = this.report;
  readonly failure = this.error;
  readonly busy = this.loading;
  reload() {
    return this.load();
  }
}

describe('permissions retry', () => {
  it('recovers from failure and clears the error after a fresh permission report', async () => {
    const invoke = vi.fn().mockRejectedValueOnce({ error: { detail: 'Unavailable' } });
    TestBed.configureTestingModule({ providers: [{ provide: Api, useValue: { invoke } }] });
    const component = TestBed.runInInjectionContext(() => new TestPreflight());
    await component.reload();
    expect(component.failure()).toBe('Unavailable');
    expect(component.busy()).toBe(false);
    const report = {
      namespace: 'production',
      ok: false,
      missing: 1,
      checks: [{ resource: 'pods', verb: 'get', allowed: null, reason: 'Review unavailable' }],
    };
    invoke.mockResolvedValueOnce(report);
    await component.reload();
    expect(invoke).toHaveBeenLastCalledWith(getPreflight);
    expect(component.result()).toEqual(report);
    expect(component.failure()).toBeNull();
    expect(component.busy()).toBe(false);
  });
});
