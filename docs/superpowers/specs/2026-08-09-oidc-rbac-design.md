# OIDC identity and RBAC-mapped authorization — design

Date: 2026-08-09
Status: approved

## Goal

Make OIDC a first-class credential in operator mode. Today OIDC *authentication*
already works in every mode — the middleware never reads `spec_backend` or
`all_namespaces` — but *authorization* does not exist for it: a valid JWT
short-circuits the middleware and gets full admin. In a standalone install that
is one namespace. In operator mode with `all_namespaces=true` it is every
PostgresCluster in the cluster, which is a privilege-escalation hole rather than
a missing feature.

The fix reuses the SubjectAccessReview engine stage C already built. A
SubjectAccessReview evaluates RBAC against *any* user and group strings the
caller supplies — the apiserver does not have to be able to authenticate that
identity itself. So pglord validates the JWT (as now), maps its claims to a
Kubernetes username and groups, and asks the same SAR the Kubernetes mode asks.
Operators bind ordinary Roles to `oidc:alice@corp.com` or group
`oidc:platform-team`. This is stage C's principle — one permission model, no
second user database — finally applied to OIDC.

Alongside it: a `whoami` endpoint so a 403 is debuggable, and the correctness
fixes the OIDC path has been carrying since it shipped.

## Decisions (settled with user)

| Question | Decision |
|---|---|
| How does an OIDC identity get authorized? | Claims → Kubernetes username + groups → the existing SAR on `pglord.dev/postgresclusters`. No impersonation, no new permission model. |
| Does the apiserver need `--oidc-issuer-url`? | No. SAR accepts arbitrary subject strings. Matching the apiserver's prefixes is a recommendation, not a requirement. |
| Default posture | Opt-in: `PGLORD_AUTH_OIDC_RBAC=false`. Existing installs keep working; a startup `warn!` fires when OIDC is on with `all_namespaces` and RBAC is off. |
| How does the UI show who is signed in? | New `GET /api/v1/auth/whoami` returning the *effective* identity as pglord resolved it — works for all three modes, and is what makes a 403 diagnosable. |
| Is the ID token used? | No. Code+PKCE with `state` needs no `nonce` when the ID token is never read; `whoami` supplies the display name from the already-validated access token. |

## Mode and configuration

Six new keys, all `#[serde(default)]` on `Config`:

| Key | Default | Meaning |
|---|---|---|
| `auth_oidc_rbac` | `false` | Authorize OIDC credentials through SAR instead of granting full admin. |
| `auth_oidc_username_claim` | `sub` | Claim carrying the username. Commonly `email` or `preferred_username`. |
| `auth_oidc_username_prefix` | `oidc:` | Prepended to the username. Empty string = no prefix. |
| `auth_oidc_groups_claim` | `groups` | Claim carrying group membership. |
| `auth_oidc_groups_prefix` | `oidc:` | Prepended to every group. Empty string = no prefix. |
| `auth_oidc_scopes` | `openid profile email offline_access` | Scopes the SPA requests. Served to the UI through `/api/v1/auth/config`. |

Prefixes exist to mirror the apiserver's `--oidc-username-prefix` /
`--oidc-groups-prefix`. Set them to whatever your cluster uses and pglord's
authorization becomes byte-identical to `kubectl`'s; leave the default and
`oidc:` is a clear, collision-free namespace for RoleBinding subjects.

Startup validation: `auth_oidc_rbac` without `auth_oidc_issuer` fails at
startup — it is a misconfiguration, not a mode. `auth_oidc_rbac` needs the same
`system:auth-delegator` binding the Kubernetes mode needs (SAR only; TokenReview
is unused on this path, but the standard ClusterRole grants both and splitting
it buys nothing).

The warning, mirroring the existing "authentication is DISABLED" precedent:

> OIDC credentials have full admin across every namespace — set
> `PGLORD_AUTH_OIDC_RBAC=true` and bind Roles to the mapped identities

## Claims → Kubernetes subject

A pure, unit-tested mapping in `auth/kubernetes.rs`:

```rust
pub struct Subject { pub user: String, pub groups: Vec<String> }

pub struct SubjectMapping {
    username_claim: String, username_prefix: String,
    groups_claim: String,   groups_prefix: String,
}

impl SubjectMapping {
    /// None = the token carries no usable identity.
    pub fn subject(&self, claims: &serde_json::Value) -> Option<Subject>;
    pub fn display_name(&self, claims: &serde_json::Value) -> Option<String>;
}
```

Rules:

- The username claim must be a non-empty string. Missing, empty, or non-string
  → `None` → 401. Authorizing an empty username is how a mistyped claim name
  turns into an accidental grant, so it is a rejection, not a fallback.
- The groups claim accepts an array of strings *or* a single string (IdPs
  disagree); anything else, or absent, yields no groups. No groups is legal —
  the user may be bound directly.
- Prefixes are applied verbatim, including the empty string. Nothing is
  lowercased or normalized: RBAC subject matching is byte-exact, so silently
  rewriting an identity would produce grants the operator never wrote.
- `display_name` reads `name`, then `preferred_username`, then `email`. Display
  only — never used for authorization.

## One authorizer, two identity sources

`KubernetesAuthorizer` currently reads groups back out of its own TokenReview
cache inside `authorize()`. That is unusable for OIDC (there is no TokenReview
entry) and is already a latent bug: if the 60s token entry expires between
`authenticate()` and `authorize()`, the SAR runs with empty groups and may deny.
Both are fixed by passing the subject in explicitly:

```rust
pub async fn authenticate(&self, token: &str) -> Option<Subject>;          // was Option<String>
pub async fn authorize(&self, cache_key: &str, subject: &Subject, access: &Access) -> bool;
```

`authorize` becomes indifferent to where the subject came from. The grant cache
key stays `(hash, verb, ns)` with the same 10s TTL, so a revoked RoleBinding
takes effect within seconds on both paths.

`AuthState` grows to name the two paths separately:

```rust
pub struct AuthState {
    tokens: Vec<String>,
    pub oidc: Option<jwks::OidcValidator>,
    /// Claim->subject mapping; Some when OIDC RBAC is on.
    pub oidc_rbac: Option<kubernetes::SubjectMapping>,
    /// Kubernetes bearer tokens are accepted (TokenReview).
    pub k8s_tokens: bool,
    /// Shared review engine. None without a Kubernetes environment — tokens
    /// then fall through to 401, never a 500.
    pub reviews: Option<kubernetes::KubernetesAuthorizer>,
}
```

`OidcValidator::validate` returns `Option<serde_json::Value>` (the validated
claims) instead of `bool`. It already decodes them and throws them away.

## Middleware: the OIDC branch

Order is unchanged — static, then OIDC, then Kubernetes. Only the OIDC arm
gains a body:

```rust
if let Some(oidc) = &auth.oidc && let Some(claims) = oidc.validate(token).await {
    let Some(mapping) = &auth.oidc_rbac else {
        return next.run(req).await;              // RBAC off: full admin, as before
    };
    let (Some(subject), Some(reviews)) = (mapping.subject(&claims), &auth.reviews) else {
        return ApiError::unauthorized("auth_invalid").into_response();
    };
    let access = kubernetes::authz_target(req.method(), req.uri().path());
    if reviews.authorize(&token_key(token), &subject, &access).await {
        return next.run(req).await;              // + Identity in request extensions
    }
    return ApiError::new(StatusCode::FORBIDDEN, "forbidden")
        .with_detail(kubernetes::denial_detail(&subject.user, &access))
        .into_response();
}
```

Every accepting branch inserts an `Identity { mode, subject, display_name, rbac }`
into request extensions. Nothing but `whoami` reads it today; it costs one
`Arc` clone and is the seam any future audit logging needs.

The verb mapping, the cluster-wide `list` for `GET /api/v1/clusters`, and the
`Access::Authenticated` passthrough for `ping`/`version`/`gucs`/`preflight` are
reused unchanged — OIDC and Kubernetes identities are authorized identically.

## `GET /api/v1/auth/whoami`

Authenticated (not public — it reports an identity). Returns the identity as
pglord resolved it, which is precisely what a confusing 403 needs:

```json
{ "mode": "oidc", "user": "oidc:alice@corp.com",
  "groups": ["oidc:platform-team"], "display_name": "Alice Ng", "rbac": true }
```

`mode` is `static` | `oidc` | `kubernetes` | `none` (auth disabled). Static
tokens report an empty user and `rbac: false`. Seeing `oidc:alice@corp.com` when
the RoleBinding says `alice@corp.com` explains the denial in one glance —
without it, prefix and claim mismatches are near-undebuggable.

## Errors

| Situation | Response |
|---|---|
| Valid JWT, RBAC off | Passes with full admin (documented legacy behavior) |
| Valid JWT, RBAC on, username claim missing/empty/non-string | 401 `auth_invalid` |
| Valid JWT, RBAC on, no Kubernetes environment | 401 `auth_invalid` — never a 500 |
| Valid JWT, RBAC on, SAR denies | 403 `forbidden`, detail naming mapped user, verb, namespace |
| Valid JWT, RBAC on, SAR call fails | 403 `forbidden` — denial is the safe default, `warn!` server-side |
| `auth_oidc_rbac` without an issuer | Startup failure |

## SPA correctness fixes

Three defects the OIDC path has carried since it shipped, all in scope here
because they are on the same code paths:

- **Scopes are hardcoded** to `openid profile` (`oidc.service.ts:53`), so most
  IdPs issue no refresh token and the entire refresh timer is dead code. Scopes
  move to `auth_oidc_scopes`, delivered through `OidcPublicConfig.scopes`.
- **`AuthService.enabled()` ignores `kubernetes_enabled`** (`auth.service.ts:36-39`),
  so a Kubernetes-only install never routes to `/login` and users get raw 401s.
  `LoginComponent.load()` has the identical gap.
- **No RP-initiated logout.** `end_session_endpoint` is captured from discovery
  at login and stored with the credential; logout redirects there with
  `client_id` and `post_logout_redirect_uri` when present, and falls back to the
  current local-only clear when absent.

The shell header shows `display_name` (falling back to `user`) beside the
existing sign-out button. All strings through Transloco.

## Deployment (both paths, kept in sync)

Chart `values.yaml` gains `auth.oidc.rbac`, `.usernameClaim`, `.usernamePrefix`,
`.groupsClaim`, `.groupsPrefix`, `.scopes`; the env block emits each only when
non-default. The `system:auth-delegator` binding condition widens from
`auth.kubernetes` to `auth.kubernetes or auth.oidc.rbac`. `deploy/k8s/deployment.yaml`
gets the matching commented stanza. `preflight::required` appends the review
checks when either RBAC path is on, so the Permissions page tells the truth in
an OIDC-RBAC install.

## Testing

**Unit:** `SubjectMapping` — claim selection, both prefixes, empty prefix,
array-vs-string groups, absent groups, missing/empty/non-string username,
`display_name` fallback order. `OidcValidator::validate` returns claims on the
existing mock-IdP suite. Middleware matrix gains: RBAC on with no kube env → 401;
RBAC off → passthrough. Config: `auth_oidc_rbac` without issuer fails; defaults.

**Integration (`tests/http.rs`):** OIDC RBAC on without a Kubernetes environment
answers 401 and never 500; `whoami` reports `static` for a static token and
`none` when auth is off; `whoami` and the new `AuthConfigResponse.scopes` are in
the OpenAPI contract.

**UI:** the two untested files finally get specs. `oidc.service.spec.ts` —
S256 challenge against a known-answer vector, state mismatch aborts, rotating
refresh token is preserved, refresh failure logs out and redirects, scopes come
from config. `auth.guard.spec.ts` — Kubernetes-only config redirects to `/login`
(the bug above), disabled auth passes, unreachable config passes.

No new e2e. The in-process mock IdP in `auth/jwks.rs` already covers the JWT
half, and standing up an IdP on the dev stand would buy coverage of Keycloak
rather than of pglord.

## Out of scope (deliberate)

Impersonation for the *write* path — pglord still acts as its own ServiceAccount ·
per-instance or sub-resource granularity · audit logging beyond the `Identity`
extension seam · UI role management or group-based feature flags · client-credentials
service accounts · ID-token parsing in the browser · moving credentials out of
`localStorage` · e2e against a real IdP.
