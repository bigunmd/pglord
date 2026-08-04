#!/usr/bin/env bash
# Extensions e2e: per-database CREATE EXTENSION scoping, preload lifecycle
# through pending_restart -> restart, and leave-in-place removal semantics.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18100

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

psql_db() { # pod db sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -d "$2" -tAc "$3" 2>&1 || true
}
detail() { curl -s "$API/api/v1/clusters/demo"; }
jsonq() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null || echo ""; }
patch() { # json
  curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/demo" \
    -H 'content-type: application/json' -d "$1"
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
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18100:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster with a declared role + database"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d '{"name":"demo","replicas":2,"roles":[{"name":"app_rw"}],"databases":[{"name":"appdb","owner":"app_rw"}]}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
ready=""
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: never 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
PRIMARY=$(detail | jsonq "d['primary']")
# appdb must exist before extensions target it
for _ in $(seq 1 60); do
  has_db=$(psql_db "$PRIMARY" postgres "SELECT 1 FROM pg_database WHERE datname='appdb'")
  [ "$has_db" = "1" ] && break
  sleep 2
done
[ "$has_db" = "1" ] || { echo "FAIL: appdb never provisioned"; exit 1; }
echo "OK cluster + appdb ready"

echo "==> pgcrypto in appdb only"
code=$(patch '{"extensions":[{"name":"pgcrypto","database":"appdb"}]}')
[ "$code" = "200" ] || { echo "FAIL: extensions PATCH returned $code"; exit 1; }
got=""
for _ in $(seq 1 60); do
  got=$(psql_db "$PRIMARY" appdb "SELECT length(digest('x','sha256'))")
  [ "$got" = "32" ] && break
  sleep 2
done
[ "$got" = "32" ] || { echo "FAIL: pgcrypto never usable in appdb (got '$got')"; exit 1; }
elsewhere=$(psql_db "$PRIMARY" postgres "SELECT length(digest('x','sha256'))")
echo "$elsewhere" | grep -q "does not exist" \
  || { echo "FAIL: pgcrypto leaked into postgres db: $elsewhere"; exit 1; }
echo "OK per-database scoping"

echo "==> pg_stat_statements with preload -> pending_restart -> restart"
code=$(patch '{"extensions":[{"name":"pgcrypto","database":"appdb"},{"name":"pg_stat_statements","database":"postgres","preload":true}]}')
[ "$code" = "200" ] || { echo "FAIL: preload PATCH returned $code"; exit 1; }
pending=""
for _ in $(seq 1 60); do
  pending=$(detail | jsonq "d['pending_restart']")
  [ "$pending" = "True" ] && break
  sleep 2
done
[ "$pending" = "True" ] || { echo "FAIL: pending_restart never surfaced"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/restart")
[ "$code" = "202" ] || { echo "FAIL: restart returned $code"; exit 1; }
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: never recovered from restart"; exit 1; }
# The roll may have moved the primary; re-resolve.
PRIMARY=$(detail | jsonq "d['primary']")
spl=""
for _ in $(seq 1 60); do
  spl=$(psql_db "$PRIMARY" postgres "SHOW shared_preload_libraries")
  echo "$spl" | grep -q "pg_stat_statements" && break
  sleep 2
done
echo "$spl" | grep -q "pg_stat_statements" || { echo "FAIL: preload not applied ($spl)"; exit 1; }
count=""
for _ in $(seq 1 60); do
  count=$(psql_db "$PRIMARY" postgres "SELECT count(*) >= 0 FROM pg_stat_statements")
  [ "$count" = "t" ] && break
  sleep 2
done
[ "$count" = "t" ] || { echo "FAIL: pg_stat_statements not queryable ($count)"; exit 1; }
echo "OK preload lifecycle"

echo "==> removal is leave-in-place for DDL, declarative for preload"
code=$(patch '{"extensions":[]}')
[ "$code" = "200" ] || { echo "FAIL: removal PATCH returned $code"; exit 1; }
sleep 5
still=$(psql_db "$PRIMARY" appdb "SELECT length(digest('x','sha256'))")
[ "$still" = "32" ] || { echo "FAIL: pgcrypto dropped on removal (got '$still')"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/restart")
[ "$code" = "202" ] || { echo "FAIL: second restart returned $code"; exit 1; }
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: never recovered from second restart"; exit 1; }
PRIMARY=$(detail | jsonq "d['primary']")
spl=$(psql_db "$PRIMARY" postgres "SHOW shared_preload_libraries")
echo "$spl" | grep -q "pg_stat_statements" \
  && { echo "FAIL: preload list kept removed entry ($spl)"; exit 1; }
echo "OK removal semantics"

echo "PASS: extensions e2e"
