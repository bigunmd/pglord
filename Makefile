IMAGE := pglord-server:dev
PROFILE := pglord
KUBECTL := minikube -p $(PROFILE) kubectl --

.DEFAULT_GOAL := help

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Build workspace (debug)
	cargo build --workspace

run: ## Run server locally
	cargo run -p pglord-server

release: ## Build workspace (release)
	cargo build --workspace --release

test: ## Run all tests
	cargo test --workspace

fmt: ## Format code
	cargo fmt --all

lint: ## fmt --check + clippy -D warnings
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings

openapi: ## Regenerate spec/openapi.yaml
	cargo run -p pglord-server -- --dump-openapi

crd: ## Regenerate deploy/crds from code
	cargo run -p pglord-server -- --dump-crd

crd-check: crd ## Fail if the committed CRD manifest drifts from code
	git diff --exit-code deploy/crds

openapi-check: openapi ## Fail if committed spec drifts from code
	git diff --exit-code spec/

image: ## Build server docker image
	docker build -f deploy/Dockerfile -t $(IMAGE) .

stand-up: ## Start minikube dev stand (profile: pglord)
	minikube start -p $(PROFILE) --driver=docker

# minikube keeps cached tags, and `image rm` fails while a running pod uses
# the image — scale the server down before swapping (see scripts/cluster-e2e.sh).
stand-image: image ## Build + load server image into minikube
	$(KUBECTL) -n pglord scale deploy/pglord-server --replicas=0 2>/dev/null || true
	$(KUBECTL) -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s 2>/dev/null || true
	minikube -p $(PROFILE) image rm docker.io/library/$(IMAGE) || true
	minikube -p $(PROFILE) image load $(IMAGE)

stand-deploy: stand-image ## Deploy pglord into the stand
	$(KUBECTL) apply -f deploy/k8s/namespace.yaml
	$(KUBECTL) apply -f deploy/k8s/

stand-smoke: ## Port-forward and curl healthz/readyz/ping
	./scripts/stand-smoke.sh

stand-ui: ## Port-forward the UI to http://localhost:8080
	@echo "pglord UI: http://localhost:8080  (Ctrl-C to stop)"
	$(KUBECTL) -n pglord port-forward svc/pglord-server 8080:80

stand-down: ## Delete the minikube stand
	minikube delete -p $(PROFILE)

agent-image: ## Build agent docker image (postgres:17 base)
	docker build -f deploy/Dockerfile.agent -t pglord-agent:dev .

agent-e2e: ## Docker e2e: replication + promotion with two agents
	./scripts/agent-e2e.sh

cluster-e2e: ## Minikube e2e: cluster via REST API with streaming replication
	./scripts/cluster-e2e.sh

failover-e2e: ## Minikube e2e: kill primary -> promote, repoint, rejoin
	./scripts/failover-e2e.sh

day2-e2e: ## Minikube e2e: planned switchover and scale-up
	./scripts/day2-e2e.sh

hibernate-e2e: ## Minikube e2e: fence/unfence a standby, hibernate/wake with PVCs kept
	./scripts/hibernate-e2e.sh

operator-e2e: ## Minikube e2e: PostgresCluster CRD backend — kubectl + REST converge
	./scripts/operator-e2e.sh

multi-ns-e2e: ## Minikube e2e: cluster-wide operator, same-named clusters in two namespaces
	./scripts/multi-ns-e2e.sh

rbac-auth-e2e: ## Minikube e2e: kubernetes tokens via TokenReview + SAR authorization
	./scripts/rbac-auth-e2e.sh

extensions-e2e: ## Minikube e2e: declared extensions — per-db scoping and preload lifecycle
	./scripts/extensions-e2e.sh

upgrade-e2e: ## Minikube e2e: blue-green major upgrade 16->17 (rollback, then finalize)
	./scripts/upgrade-e2e.sh

config-e2e: ## Minikube e2e: live settings, pending restart, rolling restart
	./scripts/config-e2e.sh

pooler-e2e: ## Minikube e2e: pgbouncer pool, auth_query, failover, disable
	./scripts/pooler-e2e.sh

provision-e2e: ## Minikube e2e: declared roles/databases converge and survive
	./scripts/provision-e2e.sh

workload-e2e: ## Minikube e2e: labels, resources and anti-affinity on pods
	./scripts/workload-e2e.sh

backup-e2e: ## Minikube e2e: back up to MinIO and verify objects land
	./scripts/backup-e2e.sh

restore-e2e: ## Minikube e2e: restore into a new cluster, and to a point in time
	./scripts/restore-e2e.sh

secrets-e2e: ## Minikube e2e: own credentials secret, custom env and init scripts
	./scripts/secrets-e2e.sh

tls-e2e: ## Minikube e2e: tls handshake, cert rotation without restart, hostnossl reject
	./scripts/tls-e2e.sh

ui-install: ## Install UI dependencies
	cd ui && npm ci

ui-dev: ## Run UI dev server (:4200)
	cd ui && npm start

ui-build: ## Build UI for production
	cd ui && npm run build

ui-lint: ## Lint UI
	cd ui && npm run lint

ui-test: ## Run UI unit tests
	cd ui && npm test

ui-client: ## Regenerate TS API client from spec/openapi.yaml
	cd ui && npm run gen:api

ui-client-check: ui-client ## Fail if committed TS client drifts from spec
	git diff --exit-code ui/src/app/api

clean: ## Remove build artifacts
	cargo clean
