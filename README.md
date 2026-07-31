# pglord

Control plane for HA PostgreSQL clusters on Kubernetes: deploy, control, and monitor
highly-available Postgres from a web UI or REST API — standalone in your own namespace
(no cluster-admin required) or, later, as a cluster operator.

Status: early foundation. See `docs/superpowers/specs/` for the design and
`docs/superpowers/plans/` for implementation plans.

## Development

```bash
make help          # list all targets
make run           # run the server locally (http://localhost:8080)
make test          # run all tests
make lint          # fmt --check + clippy -D warnings
make openapi       # regenerate spec/openapi.yaml
make stand-up      # start the minikube dev stand
make stand-deploy  # build image, load into minikube, deploy
make stand-smoke   # curl healthz/readyz/ping through a port-forward
```

## Configuration

Layered: serde defaults ← `config.toml` ← `PGLORD_*` environment variables.

| Variable | Default | Description |
|---|---|---|
| `PGLORD_HTTP_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `PGLORD_LOG_LEVEL` | `info` | tracing EnvFilter directive |
| `PGLORD_SHUTDOWN_TIMEOUT_SECS` | `25` | graceful drain budget (< k8s 30s grace) |
| `PGLORD_CORS_ALLOWED_ORIGINS` | *(empty)* | comma-separated exact origins; empty disables CORS |

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
