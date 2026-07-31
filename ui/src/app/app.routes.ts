import { Routes } from '@angular/router';

import { ShellComponent } from './shell/shell.component';

export const routes: Routes = [
  {
    path: '',
    component: ShellComponent,
    children: [
      {
        path: '',
        loadComponent: () => import('./pages/home/home.component').then((m) => m.HomeComponent),
      },
      {
        path: 'clusters',
        loadComponent: () =>
          import('./pages/clusters/clusters.component').then((m) => m.ClustersComponent),
      },
      {
        path: 'preflight',
        loadComponent: () =>
          import('./pages/preflight/preflight.component').then((m) => m.PreflightComponent),
      },
      {
        path: 'clusters/:name',
        loadComponent: () =>
          import('./pages/clusters/cluster-detail.component').then(
            (m) => m.ClusterDetailComponent,
          ),
      },
    ],
  },
];
