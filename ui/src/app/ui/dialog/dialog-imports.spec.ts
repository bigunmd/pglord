import { describe, expect, it } from 'vitest';

/**
 * Angular silently ignores an attribute directive a component forgot to
 * import: the attribute stays in the DOM, no directive runs, and nothing
 * errors. For dialogs that meant a body with no padding and no scrolling,
 * which looked like a CSS bug and was not one.
 *
 * These directives are declared per component and several dialogs share a
 * file, so "it works in this file" proves nothing about its neighbours.
 */
const SOURCES = import.meta.glob('../../pages/**/*.ts', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/** Each `@Component({...})` block, so directives are checked per component. */
function components(source: string): string[] {
  return source.split('@Component({').slice(1);
}

const DIALOG_PARTS = [
  { attribute: 'hlmDialogBody', directive: 'HlmDialogBody' },
  { attribute: 'hlmDialogHeader', directive: 'HlmDialogHeader' },
  { attribute: 'hlmDialogFooter', directive: 'HlmDialogFooter' },
  { attribute: 'hlmDialogTitle', directive: 'HlmDialogTitle' },
];

describe('dialog directives are imported by the components that use them', () => {
  it('finds component sources to check', () => {
    expect(Object.keys(SOURCES).length).toBeGreaterThan(5);
  });

  for (const { attribute, directive } of DIALOG_PARTS) {
    it(`every template using ${attribute} imports ${directive}`, () => {
      const offenders: string[] = [];
      for (const [path, source] of Object.entries(SOURCES)) {
        if (path.endsWith('.spec.ts')) {
          continue;
        }
        components(source).forEach((block, index) => {
          const usesAttribute = new RegExp(`\\b${attribute}\\b`).test(block);
          // The imports array sits before `template:`, so slicing there keeps
          // the attribute's own occurrence from counting as its import.
          const declares = new RegExp(`\\b${directive}\\b`).test(block.split('template:')[0]);
          if (usesAttribute && !declares) {
            offenders.push(`${path} (component #${index + 1})`);
          }
        });
      }
      expect(offenders, `missing ${directive} in imports`).toEqual([]);
    });
  }
});
