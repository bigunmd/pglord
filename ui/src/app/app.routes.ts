import { Routes } from '@angular/router';

import { authGuard } from './core/auth/auth.guard';
import { ShellComponent } from './shell/shell.component';

export const routes: Routes = [
  {
    path: 'login',
    loadComponent: () => import('./pages/login/login.component').then((m) => m.LoginComponent),
  },
  {
    path: 'login/callback',
    loadComponent: () =>
      import('./pages/login/callback.component').then((m) => m.LoginCallbackComponent),
  },
  {
    path: '',
    component: ShellComponent,
    canActivate: [authGuard],
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
        path: 'namespaces/:ns/clusters/:name',
        loadComponent: () =>
          import('./pages/clusters/cluster-detail.component').then(
            (m) => m.ClusterDetailComponent,
          ),
      },
    ],
  },
];
