import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import { RouterLink } from '@angular/router';
import { TranslocoModule } from '@jsverse/transloco';
import { NgIcon, provideIcons } from '@ng-icons/core';
import { lucideChevronRight } from '@ng-icons/lucide';

/** One step in the trail. The last crumb has no link: it is where you are. */
export interface Crumb {
  label: string;
  link?: string;
}

/**
 * Orientation for the editing pages, which sit two levels below the cluster
 * list. Web guidance is to use breadcrumbs once a hierarchy is three levels
 * deep, which is exactly `Clusters › <cluster> › <section>`.
 */
@Component({
  selector: 'pg-breadcrumbs',
  imports: [RouterLink, NgIcon, TranslocoModule],
  providers: [provideIcons({ lucideChevronRight })],
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <nav [attr.aria-label]="ariaLabel() || ('ux.forms.breadcrumb' | transloco)" class="mb-5">
      <ol class="text-muted-foreground flex flex-wrap items-center gap-1 text-sm">
        @for (crumb of crumbs(); track crumb.label; let last = $last) {
          <li class="flex items-center gap-1">
            @if (crumb.link && !last) {
              <a
                [routerLink]="crumb.link"
                class="hover:text-foreground rounded underline-offset-4 hover:underline"
              >
                {{ crumb.label }}
              </a>
            } @else {
              <!-- aria-current marks the page for screen readers; sighted users
                   get the same cue from the weight change. -->
              <span class="text-foreground font-medium" aria-current="page">{{ crumb.label }}</span>
            }
            @if (!last) {
              <ng-icon name="lucideChevronRight" size="14" class="opacity-60" aria-hidden="true" />
            }
          </li>
        }
      </ol>
    </nav>
  `,
})
export class BreadcrumbsComponent {
  public readonly crumbs = input.required<Crumb[]>();
  public readonly ariaLabel = input('');
}
