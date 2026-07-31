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

openapi-check: openapi ## Fail if committed spec drifts from code
	git diff --exit-code spec/

image: ## Build server docker image
	docker build -f deploy/Dockerfile -t $(IMAGE) .

stand-up: ## Start minikube dev stand (profile: pglord)
	minikube start -p $(PROFILE) --driver=docker

stand-image: image ## Build + load server image into minikube
	minikube -p $(PROFILE) image load --overwrite $(IMAGE)

stand-deploy: stand-image ## Deploy pglord into the stand
	$(KUBECTL) apply -f deploy/k8s/namespace.yaml
	$(KUBECTL) apply -f deploy/k8s/

stand-smoke: ## Port-forward and curl healthz/readyz/ping
	./scripts/stand-smoke.sh

stand-down: ## Delete the minikube stand
	minikube delete -p $(PROFILE)

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
