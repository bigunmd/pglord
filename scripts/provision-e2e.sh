#!/usr/bin/env bash
# Provisioning e2e: declared roles/databases converge on the primary — with
# passwords from referenced Secrets, attribute/membership convergence without
# restarts, leave-in-place removal, required-secrets flagging, Secret-edit
# password rotation, and survival across failover. Composes with the pooler.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18100
CL=prov

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/$CL" >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete secret app-creds --ignore-not-found >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

psql_on() { $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }
# Network path as an app role, via a service (rw or pool), from inside the cluster.
role_psql() { $KUBECTL -n pglord exec ${CL}-0 -- env PGPASSWORD="$1" psql -h "$2" -U "$3" -d "$4" -tAc "$5" 2>/dev/null || echo "DENIED"; }
restarts_of() { $KUBECTL -n pglord get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?"; }
detail() { curl -s "$API/api/v1/clusters/$CL"; }
field() { detail | python3 -c "import sys,json; print(json.load(sys.stdin)$1)" 2>/dev/null || echo ""; }

echo "==> building + loading images"
make image agent-image >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done
# Verify the stand really runs what was just built (stale-image trap).
for img in pglord-server:dev pglord-agent:dev; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img (have $have, want $want) — delete leftover clusters and retry"; exit 1; }
done
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18100:80 >/dev/null 2>&1 &
sleep 2

echo "==> invalid declarations are rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"$CL\",\"roles\":[{\"name\":\"postgres\"}]}")
[ "$code" = "422" ] || { echo "FAIL: reserved role name returned $code"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"$CL\",\"databases\":[{\"name\":\"appdb\",\"owner\":\"ghost\"}]}")
[ "$code" = "422" ] || { echo "FAIL: undeclared owner returned $code"; exit 1; }
echo "OK reserved name and dangling owner rejected as 422"

echo "==> creating cluster '$CL' (replicas=2, pooler, declared role+db)"
$KUBECTL -n pglord create secret generic app-creds --from-literal=password=app-pw-1 >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\": \"$CL\", \"replicas\": 2,
    \"pooler\": { \"enabled\": true },
    \"roles\": [
      { \"name\": \"reporting\", \"login\": false },
      { \"name\": \"app_rw\",
        \"password_secret\": { \"name\": \"app-creds\", \"key\": \"password\" },
        \"in_roles\": [\"reporting\"] }
    ],
    \"databases\": [ { \"name\": \"appdb\", \"owner\": \"app_rw\" } ]
  }")
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field "['ready_replicas']")" = "2" ] && break
  sleep 2
done
[ "$(field "['ready_replicas']")" = "2" ] || { echo "FAIL: never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret ${CL}-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
echo "OK cluster ready"

echo "==> declared role connects to the declared database"
for _ in $(seq 1 60); do
  [ "$(role_psql app-pw-1 ${CL}-rw app_rw appdb 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(role_psql app-pw-1 ${CL}-rw app_rw appdb 'SELECT 1')" = "1" ] \
  || { echo "FAIL: app_rw cannot connect to appdb via -rw"; exit 1; }
echo "OK role + password + database + owner converged"

echo "==> the same role works through the pooler (auth_query composes)"
$KUBECTL -n pglord rollout status deploy/${CL}-pooler --timeout=120s >/dev/null
for _ in $(seq 1 45); do
  [ "$(role_psql app-pw-1 ${CL}-pool app_rw appdb 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(role_psql app-pw-1 ${CL}-pool app_rw appdb 'SELECT 1')" = "1" ] \
  || { echo "FAIL: app_rw cannot connect via the pool"; exit 1; }
echo "OK pooled access for a declared role"

echo "==> membership was granted"
prim=$(field "['primary']")
[ "$(psql_on "$prim" "SELECT pg_has_role('app_rw','reporting','member')")" = "t" ] \
  || { echo "FAIL: app_rw is not a member of reporting"; exit 1; }
echo "OK app_rw is in reporting"

echo "==> attribute + membership changes converge without restarts"
before0=$(restarts_of ${CL}-0); before1=$(restarts_of ${CL}-1)
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' -d '{
    "roles": [
      { "name": "reporting", "login": false },
      { "name": "app_rw",
        "password_secret": { "name": "app-creds", "key": "password" },
        "createdb": true, "in_roles": [] }
    ]
  }')
[ "$code" = "200" ] || { echo "FAIL: role patch returned $code"; exit 1; }
ok=""
for _ in $(seq 1 60); do
  member=$(psql_on "$prim" "SELECT pg_has_role('app_rw','reporting','member')")
  createdb=$(psql_on "$prim" "SELECT rolcreatedb FROM pg_roles WHERE rolname='app_rw'")
  [ "$member" = "f" ] && [ "$createdb" = "t" ] && { ok=yes; break; }
  sleep 2
done
[ "$ok" = "yes" ] || { echo "FAIL: attributes not converged (member=$member createdb=$createdb)"; exit 1; }
[ "$(restarts_of ${CL}-0)" = "$before0" ] && [ "$(restarts_of ${CL}-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a provisioning change"; exit 1; }
echo "OK membership revoked, createdb granted, no restarts"

echo "==> removal leaves objects in place"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' -d '{"roles": [], "databases": []}')
[ "$code" = "200" ] || { echo "FAIL: clear returned $code"; exit 1; }
sleep 5
[ "$(psql_on "$prim" "SELECT count(*) FROM pg_roles WHERE rolname='app_rw'")" = "1" ] \
  || { echo "FAIL: app_rw dropped on removal"; exit 1; }
[ "$(psql_on "$prim" "SELECT count(*) FROM pg_database WHERE datname='appdb'")" = "1" ] \
  || { echo "FAIL: appdb dropped on removal"; exit 1; }
[ "$(role_psql app-pw-1 ${CL}-rw app_rw appdb 'SELECT 1')" = "1" ] \
  || { echo "FAIL: app_rw lost access after removal"; exit 1; }
echo "OK role and database survive unmanaged"

echo "==> a missing password secret is flagged, not fatal"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' -d '{
    "roles": [{ "name": "app_rw", "password_secret": { "name": "ghost-creds", "key": "password" } }]
  }')
[ "$code" = "200" ] || { echo "FAIL: ghost-secret patch returned $code"; exit 1; }
satisfied=$(curl -s "$API/api/v1/clusters/$CL/secrets" | python3 -c "import sys,json; print(json.load(sys.stdin)['satisfied'])")
[ "$satisfied" = "False" ] || { echo "FAIL: missing ghost-creds not flagged"; exit 1; }
[ "$(field "['ready_replicas']")" = "2" ] || { echo "FAIL: cluster degraded by a missing role secret"; exit 1; }
echo "OK flagged in required-secrets, cluster healthy"

echo "==> password rotation via the referenced secret"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' -d '{
    "roles": [{ "name": "app_rw", "password_secret": { "name": "app-creds", "key": "password" } }]
  }')
[ "$code" = "200" ] || { echo "FAIL: repoint patch returned $code"; exit 1; }
$KUBECTL -n pglord create secret generic app-creds --from-literal=password=app-pw-2 \
  --dry-run=client -o yaml | $KUBECTL -n pglord apply -f - >/dev/null
ok=""
for _ in $(seq 1 60); do
  if [ "$(role_psql app-pw-2 ${CL}-rw app_rw postgres 'SELECT 1')" = "1" ]; then ok=yes; break; fi
  sleep 2
done
[ "$ok" = "yes" ] || { echo "FAIL: new password never converged"; exit 1; }
[ "$(role_psql app-pw-1 ${CL}-rw app_rw postgres 'SELECT 1')" = "DENIED" ] \
  || { echo "FAIL: old password still accepted"; exit 1; }
echo "OK secret edit rotated the role password"

echo "==> provisioning survives failover"
prim=$(field "['primary']")
$KUBECTL -n pglord delete pod "$prim" >/dev/null
ok=""
for _ in $(seq 1 120); do
  if [ "$(role_psql app-pw-2 ${CL}-rw app_rw postgres 'SELECT 1')" = "1" ]; then ok=yes; break; fi
  sleep 2
done
[ "$ok" = "yes" ] || { echo "FAIL: declared role broken after failover"; exit 1; }
echo "OK converged access after failover"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/$CL" >/dev/null
echo "provision e2e passed"
