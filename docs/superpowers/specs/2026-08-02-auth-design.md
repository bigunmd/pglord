# Auth: static bearer tokens + OIDC (resource server)

Date: 2026-08-02
Status: approved

## Goal

Protect the pglord API and UI with authentication. Two independently enableable
modes that reduce to the same wire format — `Authorization: Bearer <credential>`:

- **Static tokens** — operator-configured shared secrets. For CI, `curl`, and
  small installs. Kubernetes-dashboard-style "paste a token" login in the UI.
- **OIDC** — pglord is a pure *resource server*: it validates JWT access tokens
  issued by an external IdP (Keycloak, Dex, Auth0, Entra, …). The Angular UI
  runs the authorization-code + PKCE flow itself as a public client. The server
  stays stateless: no sessions, no cookies, no client secret, no RP flow.

## Non-goals (deliberate)

- No local username/password accounts, no user management.
- No roles: any valid credential is full admin. K8s-RBAC-mapped authz remains a
  follow-up per the design spec.
- No server-side relying-party flow (would need client secret + session state).
- No auth on `/metrics`, `/healthz`, `/readyz` (k8s-native convention; counters
  only, no secrets).

## Default posture

Opt-in. Neither mode configured → server runs open exactly as today and logs a
startup `warn!` that the API is unauthenticated. Both modes may be enabled at
once (static token for CI + OIDC for humans).

## Server

### Config (figment, `PGLORD_AUTH_*`)

| Key | Meaning |
|---|---|
| `auth_tokens` | Comma-separated static bearer tokens. Empty = static mode off. Two entries make token rotation zero-downtime: add new, roll clients, remove old. |
| `auth_oidc_issuer` | Issuer URL. Empty = OIDC off. Discovery doc is fetched from `<issuer>/.well-known/openid-configuration`. |
| `auth_oidc_client_id` | Client ID the SPA uses; served to the UI via `/api/auth/config`. |
| `auth_oidc_audience` | Expected `aud` claim. Defaults to the client ID. |

Helm mounts `auth_tokens` from a Secret (`secretKeyRef`); OIDC keys are plain
values. When `auth_oidc_issuer` is set but `auth_oidc_client_id` is empty,
config validation fails at startup (fail fast, not at first login).

### Middleware

One axum layer on the API router only — same placement pattern as the CORS
layer, applied before `/healthz`, `/readyz`, `/metrics`, and swagger-ui are
merged, so those stay open by construction. Inside the layer, exactly one
anonymous path: `GET /api/auth/config`.

Check order per request:

1. No `Authorization: Bearer` header → 401 `auth_required`.
2. Header matches a static token (constant-time compare, `subtle` crate) → OK.
3. OIDC enabled → validate as JWT: signature against cached JWKS, `exp`,
   `iss` == issuer, `aud` contains audience. Valid → OK.
4. Otherwise → 401 `auth_invalid`.

Failures are RFC 9457 problem+json via the existing `error.rs` machinery with
stable codes `auth_required` / `auth_invalid`, plus
`WWW-Authenticate: Bearer` on 401s. Auth disabled → the layer is not installed.

### JWT validation (`jsonwebtoken` + small `jwks.rs`)

The `openidconnect` crate is built for relying-party flows we don't have; we
only need resource-server validation, so: fetch discovery doc → `jwks_uri` →
cache keys in-memory. On a token with unknown `kid`, refetch once (rate-limited
to ≥1 per 60 s) to pick up rotated keys. Discovery/JWKS fetch happens lazily on
first use and never blocks startup; fetch failure → 401 `auth_invalid` (with a
server-side `warn!`), not 500. Only RS256/ES256 family algorithms accepted;
`alg: none` and HMAC are rejected outright.

### Public endpoint

`GET /api/auth/config` (in OpenAPI, so the TS client is generated):

```json
{
  "static_tokens_enabled": true,
  "oidc": { "issuer": "https://idp.example.com/realms/x", "client_id": "pglord" }
}
```

`oidc` is null when OIDC is off. Issuer and client ID are public by OIDC
design; nothing sensitive is exposed. The UI uses this to decide which login
affordances to render (or, when both fields say "off", to skip login entirely).

## UI

- **Auth service** (`core/auth/`): holds the current credential in
  `localStorage['pglord.auth']`; exposes `login`/`logout`/`token` signals.
- **HTTP interceptor**: attaches `Authorization: Bearer <token>` to every
  `/api` request when a credential exists; on a 401 response clears the
  credential and navigates to `/login`.
- **Route guard** on the shell: no credential and auth enabled (per
  `/api/auth/config`, fetched once and cached) → redirect to `/login`.
- **Login page** (`/login`, outside the shell): token-paste field (static
  mode) and/or "Sign in with <issuer host>" button (OIDC), depending on
  `/api/auth/config`. All strings via Transloco.
- **OIDC PKCE service, hand-rolled** (~250 lines): Angular 22 outruns
  third-party OIDC libs' Angular-major support, and the code-with-PKCE flow is
  small: fetch IdP discovery doc → build authorize URL (S256 challenge,
  `state`, `nonce` in sessionStorage) → redirect → `/login/callback` route
  exchanges the code at the token endpoint (public client, CORS per OIDC
  spec) → store `access_token` (+ `refresh_token` if granted; refresh ~60 s
  before expiry, else re-redirect on 401).
- **Logout**: clears storage, returns to `/login`. No IdP-side logout in v1.

## Deploy (both paths, kept in sync)

- **Helm**: `auth.tokens.existingSecret`/`auth.tokens.secretKey` (or
  `auth.tokens.value` for dev, rendered into a chart-managed Secret) →
  `PGLORD_AUTH_TOKENS`; `auth.oidc.issuer`/`clientId`/`audience` → plain env.
- **Raw manifests**: commented-out env stanzas in `deploy/k8s/deployment.yaml`
  showing the same wiring.

## Testing

- **Unit (server)**: config parsing/validation; middleware allow/deny matrix
  (no header, bad token, good token, valid/expired/wrong-aud/wrong-iss JWT,
  unknown-kid refetch, alg confusion rejected). JWTs minted in-test with a
  generated RSA key served from a mock JWKS endpoint.
- **Integration**: `app::build()` with auth on → `/api/clusters` 401 without
  and 200 with credential; `/api/auth/config`, `/healthz`, `/metrics` open;
  auth off → everything open (back-compat).
- **UI**: interceptor attaches header and handles 401; guard redirects.
- **E2e**: existing suites unchanged (stand runs authless by default). A small
  `auth-e2e` (static token: 401 then 200) is a follow-up, not in scope.

## Docs

`docs/user-guide.md` gains an "Authentication" section: enabling static
tokens, rotation, OIDC setup (issuer/client/audience, redirect URI
`<ui-origin>/login/callback`, example Keycloak client settings).
