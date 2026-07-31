# pglord UI Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Angular SPA foundation in `ui/`: required libs, Postgres-colored light/dark theming, Transloco i18n (en), generated TS API client from `spec/openapi.yaml`, and a minimal shell proving the whole chain against the live Rust server.

**Architecture:** Mirrors `~/github/signal/web`: Angular 22 standalone + signals (no NgRx), Tailwind v4 CSS-first (no config file), spartan/ui vendored into `src/app/ui` via CLI with `@spartan-ng/helm/*` tsconfig path aliases, Transloco runtime catalogs under `public/i18n/`, ng-openapi-gen functional client committed under `src/app/api`. Dev mode: `ng serve` on :4200 calling the Rust server on :8080 through its CORS layer (`PGLORD_CORS_ALLOWED_ORIGINS`). Embedding into the server binary is a later slice.

**Tech Stack:** Angular ^22, @spartan-ng/brain+cli ^1.1, tailwindcss ^4, @jsverse/transloco ^8, ng-openapi-gen ^1, @ng-icons/lucide, @fontsource-variable/{inter,jetbrains-mono}, vitest.

## Global Constraints

- UI lives in `ui/` (per design doc), Angular workspace name `pglord-ui`, component prefix `pg`.
- Theme tokens: shadcn/spartan variable names as plain hex on `:root` / `:root.dark`; dark mode = `.dark` class on `<html>`; palette derived from PostgreSQL brand blue `#336791`.
- i18n from day 1: every user-visible string goes through Transloco (`en` only for now); catalogs are runtime assets in `ui/public/i18n/`.
- API access only through the generated client (`src/app/api`, committed, regenerated via `make ui-client`); eslint ignores the generated tree.
- `make ui-lint` and `make ui-build` must pass before every commit; CI gets a `ui` job.

---

### Task 1: Scaffold Angular app

**Files:** Create `ui/` via Angular CLI; adjust `ui/.prettierrc`, `ui/eslint.config.js` (add-eslint schematic), `.gitignore`.

- [ ] Step 1: `cd /home/bigunmd/github/pglord && npx -y @angular/cli@22 new pglord-ui --directory ui --style css --ssr false --routing --strict --skip-git --package-manager npm --prefix pg`
- [ ] Step 2: `cd ui && ng add @angular-eslint/schematics --skip-confirmation`
- [ ] Step 3: `.prettierrc`: `{ "printWidth": 100, "singleQuote": true, "overrides": [{ "files": "*.html", "options": { "parser": "angular" } }] }`
- [ ] Step 4: Append `ui/node_modules/`, `ui/dist/`, `ui/.angular/` to root `.gitignore`.
- [ ] Step 5: Verify `npm run build` + `npm run lint` green; commit `feat(ui): scaffold Angular 22 app`.

### Task 2: Tailwind v4 + spartan + Postgres theme

**Files:** `ui/.postcssrc.json`, `ui/src/styles.css` (full rewrite), `ui/components.json`, `ui/tsconfig.json` (paths), `ui/tsconfig.app.json` (include), `ui/eslint.config.js` (ui/** override), `ui/src/index.html` (anti-FOUC script), theme service `ui/src/app/core/theme/theme.service.ts`.

- [ ] Step 1: `npm i @spartan-ng/brain class-variance-authority clsx tailwind-merge @ng-icons/core @ng-icons/lucide @fontsource-variable/inter @fontsource-variable/jetbrains-mono && npm i -D tailwindcss @tailwindcss/postcss @spartan-ng/cli`
- [ ] Step 2: `.postcssrc.json`: `{ "plugins": { "@tailwindcss/postcss": {} } }`
- [ ] Step 3: `styles.css` — layered tailwind imports + spartan preset + Postgres palette (exact block below).
- [ ] Step 4: `components.json`: `{ "componentsPath": "src/app/ui", "importAlias": "@spartan-ng/helm", "style": "nova" }`; vendor components: `ng g @spartan-ng/cli:ui button card dropdown-menu` (or `ui-*` names per CLI); add tsconfig `paths` for each + utils; add `"src/app/ui/**/src/**/*.ts"` to tsconfig.app.json include; add eslint override block disabling selector rules for `src/app/ui/**`.
- [ ] Step 5: anti-FOUC inline script in `index.html` reading `localStorage['pglord.settings']`.
- [ ] Step 6: `ThemeService`: `mode` signal (`'system'|'light'|'dark'`), `resolved` computed with `matchMedia` listener, persists `{theme}` to `localStorage['pglord.settings']`, applies `.dark` class + `colorScheme`.
- [ ] Step 7: build + lint green; commit `feat(ui): tailwind v4 + spartan with Postgres light/dark theme`.

**styles.css theme block (Postgres palette):**

```css
@layer theme, base, components, utilities;
@import 'tailwindcss/theme.css' layer(theme);
@import 'tailwindcss/preflight.css' layer(base);
@import 'tailwindcss/utilities.css';
@import '@spartan-ng/brain/hlm-tailwind-preset.css';
@import '@fontsource-variable/inter';
@import '@fontsource-variable/jetbrains-mono';

:root {
  color-scheme: light;
  --background: #fbfcfd;
  --foreground: #10202e;
  --card: #ffffff;
  --card-foreground: #10202e;
  --popover: #ffffff;
  --popover-foreground: #10202e;
  --primary: #336791; /* PostgreSQL brand blue */
  --primary-foreground: #ffffff;
  --secondary: #e7eef4;
  --secondary-foreground: #244a68;
  --muted: #eff3f7;
  --muted-foreground: #587184;
  --accent: #dcebf5;
  --accent-foreground: #244a68;
  --destructive: #c0392b;
  --destructive-foreground: #ffffff;
  --border: #d8e1e9;
  --input: #d8e1e9;
  --ring: #336791;
  --success: #2e7d4f;
  --warning: #b7791f;
  --radius: 0.5rem;
  --font-sans: 'Inter Variable', system-ui, sans-serif;
  --font-mono: 'JetBrains Mono Variable', ui-monospace, monospace;
}

:root.dark {
  color-scheme: dark;
  --background: #0d1620; /* deep Postgres navy */
  --foreground: #dbe6ef;
  --card: #131f2b;
  --card-foreground: #dbe6ef;
  --popover: #131f2b;
  --popover-foreground: #dbe6ef;
  --primary: #6ba5cd; /* lightened Postgres blue for dark surfaces */
  --primary-foreground: #0d1620;
  --secondary: #1c2c3a;
  --secondary-foreground: #b9cbda;
  --muted: #192634;
  --muted-foreground: #8aa0b4;
  --accent: #223f55;
  --accent-foreground: #cfe2f0;
  --destructive: #e5534b;
  --destructive-foreground: #10202e;
  --border: #243849;
  --input: #243849;
  --ring: #6ba5cd;
  --success: #4caf7d;
  --warning: #d29a3d;
}

@theme inline {
  --color-success: var(--success);
  --color-warning: var(--warning);
}

@layer base {
  * { @apply border-border outline-ring/50; }
  body { @apply bg-background text-foreground font-sans; }
}
```

### Task 3: Transloco i18n (en)

**Files:** `ui/src/app/core/i18n/transloco-loader.ts`, `ui/public/i18n/en.json`, `ui/src/app/app.config.ts`.

- [ ] Step 1: `npm i @jsverse/transloco`
- [ ] Step 2: Loader: `getTranslation(lang) { return this.http.get<Translation>(`/i18n/${lang}.json`); }`
- [ ] Step 3: `provideTransloco({ config: { availableLangs: ['en'], defaultLang: 'en', reRenderOnLangChange: true, prodMode: !isDevMode() }, loader: TranslocoHttpLoader })` in app.config.ts (with `provideHttpClient()`).
- [ ] Step 4: `public/i18n/en.json` seeded with `app.title`, `nav.*`, `theme.*`, `home.*` keys.
- [ ] Step 5: build + lint; commit `feat(ui): transloco i18n with runtime en catalog`.

### Task 4: Generated API client + environments

**Files:** `ui/ng-openapi-gen.json`, `ui/src/environments/environment{,.prod}.ts` (`apiBaseUrl`), `ui/src/app/api/**` (generated), Makefile `ui-client` target, eslint ignore for `src/app/api/**`.

- [ ] Step 1: `npm i -D ng-openapi-gen`; `ng-openapi-gen.json`: `{ "input": "../spec/openapi.yaml", "output": "src/app/api", "ignoreUnusedModels": false }`; script `"gen:api": "ng-openapi-gen --config ng-openapi-gen.json"`.
- [ ] Step 2: environments: dev `apiBaseUrl: 'http://localhost:8080'`, prod `apiBaseUrl: ''` (same-origin once embedded); angular.json fileReplacements for prod.
- [ ] Step 3: Generate client; register `provideApiConfiguration(environment.apiBaseUrl)` in app.config.ts.
- [ ] Step 4: build + lint; commit `feat(ui): generated TS client from spec/openapi.yaml`.

### Task 5: Shell + home page (end-to-end proof)

**Files:** `ui/src/app/shell/shell.component.ts`, `ui/src/app/pages/home/home.component.ts`, `ui/src/app/app.routes.ts`, `ui/src/app/app.ts`.

- [ ] Step 1: Routes: `'' → Shell → { '' → Home }`; root `App` = `<router-outlet />`.
- [ ] Step 2: Shell: topbar with app title (transloco `app.title`), theme dropdown (system/light/dark, lucide monitor/sun/moon icons, spartan button+dropdown-menu), `<router-outlet />` in main.
- [ ] Step 3: Home: spartan card showing server status — calls generated `ping` + `version` fns via `Api.invoke`, renders version + reachability, all strings via transloco.
- [ ] Step 4: Manual E2E: run Rust server with `PGLORD_CORS_ALLOWED_ORIGINS=http://localhost:4200`, `ng serve`, verify home card shows version 0.1.0, theme toggle flips palette and persists across reload.
- [ ] Step 5: build + lint + test; commit `feat(ui): shell with theme toggle and server status home`.

### Task 6: Makefile + CI integration

**Files:** `Makefile` (ui targets), `.github/workflows/ci.yaml` (ui job).

- [ ] Step 1: Makefile targets: `ui-install` (npm ci), `ui-dev` (npm start), `ui-build` (npm run build), `ui-lint` (npm run lint), `ui-client` (npm run gen:api), `ui-test` (npm test).
- [ ] Step 2: CI `ui` job: checkout, setup-node 22 with npm cache (`cache-dependency-path: ui/package-lock.json`), `npm ci`, `npm run lint`, `npm run build`, plus client drift gate: `npm run gen:api && git diff --exit-code src/app/api`.
- [ ] Step 3: Commit `chore: ui make targets and ci job`.

## Verification

1. `make ui-lint ui-build ui-test` clean.
2. `make ui-client && git diff --exit-code ui/src/app/api` — committed client matches spec.
3. Manual: server on :8080 (CORS for :4200) + `make ui-dev` → home card shows version from API; theme toggle light/dark/system persists across reload (no FOUC).
