# Contributing to pglord

Thanks for taking the time. pglord is young, so almost everything is still open
to argument — including the design decisions already made. Issues that explain
why something is wrong are as welcome as patches.

## Before you write code

For anything beyond a small fix, **open an issue first**. pglord makes a few
opinionated choices (no CRDs by default, a curated GUC catalog, authorization
delegated to Kubernetes RBAC rather than a user database) and a change that
cuts against one of them needs a conversation before it needs a diff.

The reasoning behind most non-obvious decisions is written down:

- `docs/superpowers/specs/` — design specs, one per feature, with a
  "Decisions" table and an explicit "Out of scope" section.
- `docs/superpowers/plans/` — the implementation plan each spec produced.
- `docs/user-guide.md` — user-facing behaviour. If your change alters
  behaviour, this file changes with it.

If you are about to argue with a design decision, check whether the spec
already considered and rejected your alternative. If it did and the reasoning is
wrong, say so — that is a useful issue.

## Getting set up

```bash
make build          # cargo build --workspace
make run            # server on :8080, no Kubernetes needed
make ui-install     # UI dependencies
make ui-dev         # Angular dev server on :4200
make help           # everything else
```

The server runs without a Kubernetes environment: cluster endpoints answer
`503 kube_unavailable`, and everything else works. That is enough for most API
and UI work.

## The one rule that catches everyone

**Never hand-edit `spec/openapi.yaml` or anything under `ui/src/app/api/`.**

The API is code-first. `axum` + `utoipa` in `pglord-server` emit the OpenAPI
spec, and `ng-openapi-gen` generates the TypeScript client from it. Both
artifacts are committed, and CI fails if either drifts from the code.

After changing any handler or DTO:

```bash
make openapi ui-client
```

and commit the regenerated files **in the same commit** as the code that
changed them.

`app::build()` in `crates/pglord-server/src/app.rs` is the single composition
seam — it builds the router and the OpenAPI document, and is called from
`main()`, from `--dump-openapi`, and from the integration tests. Wire new routes
there and nowhere else, so the three cannot disagree.

## Before you open a pull request

Run the full gate. CI runs the same thing, and it is much faster to find out
locally:

```bash
make lint test openapi-check ui-client-check ui-lint ui-test
```

- `make lint` is `cargo fmt --all --check` plus `clippy --all-targets -D warnings`.
  Rust edition 2024. `unwrap_used` is a clippy warning, so prefer `expect` with
  a reason, or propagate the error.
- `make test` is `cargo test --workspace`. A single test:
  `cargo test -p pglord-core <name>`.
- The `*-check` targets fail on generated-artifact drift (see above).
- All user-visible UI strings go through Transloco — add keys to
  `ui/public/i18n/`, never a bare string in a template.

## Things that are easy to miss

**Two install paths must stay in sync.** `deploy/k8s/` (raw manifests) and
`charts/pglord/` (Helm chart) describe the same deployment. A change to RBAC,
the Deployment or env wiring usually needs the same change in both. Nothing
automated checks this.

`deploy/k8s/` is the **minikube dev stand**, not a production path — its images
are `:dev` tags on purpose, so the e2e suites exercise what you just built.

**Errors are RFC 9457 problem+json.** Use `ApiError` from
`crates/pglord-server/src/error.rs`. Clients key on the stable `code` field, so
adding a code is an API change; changing one is a breaking change.

**Reserved things are reserved.** The GUC catalog rejects settings HA depends
on, and workload labels reject pglord's own prefixes. If your change needs to
touch those lists, explain why in the PR.

## Tests

New behaviour needs a test. What kind depends on where it lives:

- **Pure logic** (`pglord-core`: spec validation, rendering, GUC catalog,
  backup/restore argument building) — unit tests next to the code. This is
  where most logic should live, precisely because it is cheap to test.
- **HTTP surface** — integration tests in `crates/pglord-server/tests/http.rs`,
  built through `app::build()` so they exercise the real router.
- **UI** — `*.spec.ts` beside the component or service.

**End-to-end suites are not expected on every PR.** They need a minikube stand
and take minutes each:

```bash
make stand-up stand-deploy stand-smoke
make cluster-e2e failover-e2e day2-e2e     # 19 suites in total, see make help
```

Some need extra infrastructure — `backup-e2e`, `restore-e2e` and
`upgrade-inplace-e2e` want the dev MinIO from `deploy/k8s/dev/minio.yaml`;
`agent-e2e` is Docker-only and needs no cluster. CI does not run any of them, so
if your change touches failover, upgrades or backups, running the relevant suite
locally and saying so in the PR is genuinely valuable.

## Commits and pull requests

Conventional commits, scoped by area:

```
feat(server): authorize OIDC credentials through SubjectAccessReview
fix(ui): create in the namespace the server manages, not a guessed default
docs(user-guide): authentication section
test(e2e): kubernetes rbac auth flow
```

Write the commit body for someone reading `git log` in a year: what changed and
*why*, not a restatement of the diff.

Keep pull requests focused. A refactor and a feature in one PR are two PRs.

## Reporting bugs and security issues

Bugs and feature requests go to [Issues](https://github.com/bigunmd/pglord/issues) —
the templates ask for the things needed to reproduce.

**Do not open a public issue for a security vulnerability.** See
[SECURITY.md](SECURITY.md).

## Licensing

pglord is AGPL-3.0-or-later. By contributing you agree that your contributions
are licensed under the same terms. There is no CLA.
