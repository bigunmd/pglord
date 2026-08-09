# Security Policy

## Reporting a vulnerability

**Please do not open a public issue.**

Report privately through
[GitHub Security Advisories](https://github.com/bigunmd/pglord/security/advisories/new),
or by email to **bigun.md@gmail.com**.

Please include:

- what an attacker can do, and what access they need to start
- affected version (`GET /api/v1/version`, or the chart's `appVersion`)
- the configuration it needs — auth mode, `spec_backend`, whether the server
  runs cluster-wide
- reproduction steps, ideally against the minikube stand (`make stand-up
  stand-deploy`)

pglord is maintained by one person, so please allow a few days for a first
response. You will get an acknowledgement, an assessment, and — if the report is
valid — a fix and a released version. Credit in the advisory if you want it.

## Supported versions

pglord is pre-1.0. Only the latest release gets fixes; there are no backports.

| Version | Supported |
|---|---|
| 0.2.x | Yes |
| < 0.2 | No |

## What is in scope

pglord is a control plane. It holds PostgreSQL superuser credentials, creates
Kubernetes objects on your behalf, and — in its RBAC modes — decides who may
touch which cluster. Reports in these areas are especially welcome:

- **Authentication bypass** — reaching an authenticated `/api/*` endpoint
  without a valid credential.
- **Authorization bypass** — acting in a namespace a SubjectAccessReview should
  have denied, or a request mapping to a weaker verb than the operation it
  performs (`crates/pglord-server/src/auth/kubernetes.rs`).
- **Token handling** — JWT validation, algorithm confusion, issuer or audience
  confusion, JWKS handling (`crates/pglord-server/src/auth/jwks.rs`).
- **Credential exposure** — database or object-storage credentials leaking into
  API responses, rendered manifests, logs or metrics.
- **Privilege escalation through rendered objects** — a cluster spec that makes
  pglord create RBAC, mounts or pod specs beyond what its own ServiceAccount
  should allow (`crates/pglord-core/src/render.rs`).
- **Escaping the spec's constraints** — reaching a reserved GUC, a reserved
  label prefix, or a `pg_hba` rule the validator is supposed to reject.

## What is not a vulnerability

These are documented behaviours, not bugs. Please read
[the Authentication section](docs/user-guide.md#11-authentication) before
reporting one:

- **The API is unauthenticated until you configure it.** A fresh install is
  open by design, and says so loudly in its startup log. Exposing an
  unauthenticated pglord is a deployment decision.
- **`/healthz`, `/readyz` and `/metrics` are always unauthenticated.** This
  follows Kubernetes convention. They expose counters, not secrets.
- **Static tokens and plain OIDC are full admin.** Per-namespace authorization
  requires `PGLORD_AUTH_KUBERNETES` or `PGLORD_AUTH_OIDC_RBAC`. This is stated
  in the guide and warned about at startup when running cluster-wide.
- **Credentials are readable by anyone who can already administer the
  namespace.** pglord stores them in Kubernetes Secrets; Kubernetes RBAC is the
  boundary, not pglord.
- **The UI keeps its bearer token in `localStorage`.** A deliberate trade-off
  for a stateless public-client SPA. An XSS in the UI is very much a
  vulnerability — the storage choice on its own is not.
- **Anything requiring cluster-admin to exploit.** If the attacker already has
  it, they do not need pglord.

Reports about the `pglord-agent` container image inheriting CVEs from its
upstream `postgres` base are useful, but please check whether the fix belongs
upstream first.
