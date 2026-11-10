import { describe, expect, it } from 'vitest';

// Keep the safety prerequisite reachable: primary switchover now requires fencing
// the old primary. This template contract complements server acknowledgement tests.
const sources = import.meta.glob('./cluster-detail.component.ts', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

describe('primary fencing control', () => {
  it('offers Fence for every instance not already declared fenced', () => {
    const source = sources['./cluster-detail.component.ts'];
    expect(source).toBeDefined();
    const controls = source.slice(source.indexOf("@if (i.role === 'fenced')"));
    const fenceButton = controls.slice(0, controls.indexOf('(click)="openFence(i.pod)"'));
    expect(fenceButton).toContain('} @else {');
    expect(fenceButton).not.toContain('i.pod !== d.primary');
  });
});
