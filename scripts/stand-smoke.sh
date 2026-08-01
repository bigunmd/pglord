#!/usr/bin/env bash
# Smoke-test the minikube stand: healthz + readyz + ping through a port-forward.
set -euo pipefail
KUBECTL="minikube -p pglord kubectl --"
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s
$KUBECTL -n pglord port-forward svc/pglord-server 18080:80 &
PF=$!
trap 'kill $PF 2>/dev/null || true' EXIT
sleep 2
for path in healthz readyz api/v1/ping; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:18080/$path")
  [ "$code" = "200" ] || {
    echo "FAIL /$path -> $code"
    exit 1
  }
  echo "OK /$path"
done
curl -sf http://127.0.0.1:18080/metrics | grep -q '^pglord_clusters' \
  || { echo "FAIL /metrics missing pglord_clusters"; exit 1; }
echo "OK /metrics"
echo "stand smoke passed"
