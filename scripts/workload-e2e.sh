#!/usr/bin/env bash
# Workload customization e2e: labels, resource requests and anti-affinity land
# on the pods; reserved keys and storage-class changes are refused.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18099

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/wl" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

jsonpath() { $KUBECTL -n pglord get pod wl-0 -o jsonpath="$1" 2>/dev/null || echo ""; }

echo "==> building + loading images"
make image agent-image >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18099:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster with labels, annotations and resources"
code=$(curl -s -o /tmp/wl.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d '{
    "name":"wl","replicas":1,
    "workload":{
      "labels":{"team":"payments"},
      "annotations":{"prometheus.io/scrape":"true"},
      "resources":{"cpu_request":"100m","memory_request":"256Mi","memory_limit":"1Gi"}
    }}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code: $(cat /tmp/wl.json)"; exit 1; }
for _ in $(seq 1 120); do
  [ "$(jsonpath '{.metadata.name}')" = "wl-0" ] && break
  sleep 2
done

echo "==> verifying pod metadata and resources"
[ "$(jsonpath '{.metadata.labels.team}')" = "payments" ] || { echo "FAIL: label missing"; exit 1; }
# Managed labels must survive alongside user ones.
[ "$(jsonpath '{.metadata.labels.app\.kubernetes\.io/instance}')" = "wl" ] || { echo "FAIL: managed label lost"; exit 1; }
[ "$(jsonpath '{.metadata.annotations.prometheus\.io/scrape}')" = "true" ] || { echo "FAIL: annotation missing"; exit 1; }
[ "$(jsonpath '{.spec.containers[0].resources.requests.cpu}')" = "100m" ] || { echo "FAIL: cpu request missing"; exit 1; }
[ "$(jsonpath '{.spec.containers[0].resources.requests.memory}')" = "256Mi" ] || { echo "FAIL: memory request missing"; exit 1; }
[ "$(jsonpath '{.spec.containers[0].resources.limits.memory}')" = "1Gi" ] || { echo "FAIL: memory limit missing"; exit 1; }
echo "OK labels, annotations and resources applied"

echo "==> verifying default anti-affinity spreads instances"
topo=$(jsonpath '{.spec.affinity.podAntiAffinity.preferredDuringSchedulingIgnoredDuringExecution[0].podAffinityTerm.topologyKey}')
[ "$topo" = "kubernetes.io/hostname" ] || { echo "FAIL: no preferred anti-affinity (got '$topo')"; exit 1; }
# Preferred, not required: a one-node cluster must still schedule.
[ -z "$(jsonpath '{.spec.affinity.podAntiAffinity.requiredDuringSchedulingIgnoredDuringExecution}')" ] \
  || { echo "FAIL: default anti-affinity is required, not preferred"; exit 1; }
[ "$(jsonpath '{.status.phase}')" = "Running" ] || { echo "FAIL: pod not Running"; exit 1; }
echo "OK preferred anti-affinity on hostname, pod scheduled"

echo "==> rejecting reserved keys and storage-class changes"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/wl" \
  -H 'content-type: application/json' -d '{"workload":{"labels":{"app.kubernetes.io/instance":"hijack"}}}')
[ "$code" = "422" ] || { echo "FAIL: reserved label returned $code, expected 422"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/wl" \
  -H 'content-type: application/json' -d '{"workload":{"storage_class":"other"}}')
[ "$code" = "422" ] || { echo "FAIL: storage class change returned $code, expected 422"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/wl" \
  -H 'content-type: application/json' -d '{"workload":{"resources":{"memory_request":"lots"}}}')
[ "$code" = "422" ] || { echo "FAIL: bad quantity returned $code, expected 422"; exit 1; }
echo "OK reserved keys, storage class and bad quantities refused"

echo "==> updating resources rolls the pods"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/wl" \
  -H 'content-type: application/json' -d '{"workload":{"resources":{"cpu_request":"150m","memory_request":"300Mi"},"anti_affinity":"required"}}')
[ "$code" = "200" ] || { echo "FAIL: patch returned $code"; exit 1; }
for _ in $(seq 1 90); do
  [ "$(jsonpath '{.spec.containers[0].resources.requests.cpu}')" = "150m" ] && break
  sleep 2
done
[ "$(jsonpath '{.spec.containers[0].resources.requests.cpu}')" = "150m" ] || { echo "FAIL: resources not updated"; exit 1; }
[ -n "$(jsonpath '{.spec.affinity.podAntiAffinity.requiredDuringSchedulingIgnoredDuringExecution}')" ] \
  || { echo "FAIL: anti-affinity not switched to required"; exit 1; }
echo "OK resources and anti-affinity updated"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/wl" >/dev/null
echo "workload e2e passed"
