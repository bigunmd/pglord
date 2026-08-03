#!/usr/bin/env bash
# Kubernetes-RBAC auth e2e: TokenReview + SAR gate the API. SA `alice` with
# get/list/patch on postgresclusters in team-a can work there and nowhere
# else; the static admin token bypasses RBAC; 401 vs 403 semantics hold.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18099
ADMIN="e2e-admin"

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  $KUBECTL delete ns team-a team-b --ignore-not-found --wait=false >/dev/null 2>&1 || true
  $KUBECTL delete clusterrole pglord-operator --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL delete clusterrolebinding pglord-operator pglord-auth-delegator --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL -n pglord set env deploy/pglord-server \
    PGLORD_SPEC_BACKEND=configmap PGLORD_ALL_NAMESPACES=false \
    PGLORD_AUTH_KUBERNETES- PGLORD_AUTH_TOKENS- >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

code_as() { # token method path [body]
  if [ -n "${4:-}" ]; then
    curl -s -o /dev/null -w '%{http_code}' -X "$2" "$API$3" \
      -H "Authorization: Bearer $1" -H 'content-type: application/json' -d "$4"
  else
    curl -s -o /dev/null -w '%{http_code}' -X "$2" "$API$3" -H "Authorization: Bearer $1"
  fi
}

echo "==> building + loading images"
make image agent-image >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done
for img in pglord-server:dev pglord-agent:dev; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img"; exit 1; }
done

echo "==> operator mode + kubernetes auth + admin token"
$KUBECTL apply -f deploy/crds/ >/dev/null
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL apply -f deploy/k8s/operator/rbac.yaml >/dev/null
$KUBECTL -n pglord set env deploy/pglord-server \
  PGLORD_SPEC_BACKEND=crd PGLORD_ALL_NAMESPACES=true \
  PGLORD_AUTH_KUBERNETES=true "PGLORD_AUTH_TOKENS=$ADMIN" >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18099:80 >/dev/null 2>&1 &
sleep 2

echo "==> alice: SA with get/list/patch on postgresclusters in team-a only"
$KUBECTL create ns team-a >/dev/null
$KUBECTL create ns team-b >/dev/null
$KUBECTL -n team-a create sa alice >/dev/null
$KUBECTL -n team-a apply -f - >/dev/null <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: pgc-editor
  namespace: team-a
rules:
  - apiGroups: [pglord.dev]
    resources: [postgresclusters]
    verbs: [get, list, patch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: alice-pgc-editor
  namespace: team-a
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: pgc-editor
subjects:
  - kind: ServiceAccount
    name: alice
    namespace: team-a
EOF

echo "==> clusters via the admin token"
for ns in team-a team-b; do
  code=$(code_as "$ADMIN" POST "/api/v1/namespaces/$ns/clusters" '{"name":"demo","replicas":1}')
  [ "$code" = "201" ] || { echo "FAIL: admin create in $ns returned $code"; exit 1; }
done
ready=""
for _ in $(seq 1 180); do
  ready=$(curl -s -H "Authorization: Bearer $ADMIN" "$API/api/v1/namespaces/team-a/clusters/demo" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['ready_replicas'])" 2>/dev/null || echo "")
  [ "$ready" = "1" ] && break
  sleep 2
done
[ "$ready" = "1" ] || { echo "FAIL: team-a/demo never ready"; exit 1; }
echo "OK clusters up"

TOKEN=$($KUBECTL -n team-a create token alice)
[ -n "$TOKEN" ] || { echo "FAIL: could not mint alice token"; exit 1; }

echo "==> alice allowed in team-a"
code=$(code_as "$TOKEN" GET "/api/v1/namespaces/team-a/clusters")
[ "$code" = "200" ] || { echo "FAIL: alice list team-a returned $code"; exit 1; }
code=$(code_as "$TOKEN" PATCH "/api/v1/namespaces/team-a/clusters/demo" '{"parameters":{"max_connections":"120"}}')
[ "$code" = "200" ] || { echo "FAIL: alice patch team-a returned $code"; exit 1; }
echo "OK alice works in team-a"

echo "==> alice denied everywhere else"
code=$(code_as "$TOKEN" GET "/api/v1/namespaces/team-b/clusters/demo")
[ "$code" = "403" ] || { echo "FAIL: alice get team-b returned $code, expected 403"; exit 1; }
body=$(curl -s -X GET "$API/api/v1/namespaces/team-b/clusters/demo" -H "Authorization: Bearer $TOKEN")
echo "$body" | grep -q '"forbidden"' || { echo "FAIL: wrong problem code: $body"; exit 1; }
code=$(code_as "$TOKEN" GET "/api/v1/clusters")
[ "$code" = "403" ] || { echo "FAIL: alice global list returned $code, expected 403"; exit 1; }
code=$(code_as "$TOKEN" DELETE "/api/v1/namespaces/team-a/clusters/demo")
[ "$code" = "403" ] || { echo "FAIL: alice delete returned $code, expected 403 (no delete verb)"; exit 1; }
echo "OK RBAC boundaries hold (403 forbidden)"

echo "==> 401 semantics"
code=$(curl -s -o /dev/null -w '%{http_code}' "$API/api/v1/namespaces/team-a/clusters")
[ "$code" = "401" ] || { echo "FAIL: no token returned $code, expected 401"; exit 1; }
code=$(code_as "garbage-token" GET "/api/v1/namespaces/team-a/clusters")
[ "$code" = "401" ] || { echo "FAIL: garbage token returned $code, expected 401"; exit 1; }
echo "OK 401 for missing/invalid credentials"

echo "==> admin token bypasses RBAC everywhere"
code=$(code_as "$ADMIN" GET "/api/v1/clusters")
[ "$code" = "200" ] || { echo "FAIL: admin global list returned $code"; exit 1; }
code=$(code_as "$ADMIN" DELETE "/api/v1/namespaces/team-b/clusters/demo")
[ "$code" = "200" ] || { echo "FAIL: admin delete returned $code"; exit 1; }
echo "OK static admin unaffected"

echo "PASS: kubernetes rbac auth e2e"
