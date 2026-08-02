#!/usr/bin/env bash
# Pooler e2e: PgBouncer fronts the primary via the pool Service, auth_query
# lets app roles through with their own passwords, transaction pooling shares
# server connections, failover and rotation survive under the pool, a knob
# change rolls only pooler pods, and disabling deletes the pooler objects.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18099
CL=poolt

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/$CL" >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete configmap ${CL}-init >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

# Local socket on an instance (peer path, unaffected by pooling).
psql_on() { $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }
# Network path through the pool Service, from inside the cluster.
pool_psql() { $KUBECTL -n pglord exec ${CL}-0 -- env PGPASSWORD="$1" psql -h ${CL}-pool -U "$2" -d "$3" -tAc "$4" 2>/dev/null || echo "DENIED"; }
restarts_of() { $KUBECTL -n pglord get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?"; }
detail() { curl -s "$API/api/v1/clusters/$CL"; }
field() { detail | python3 -c "import sys,json; print(json.load(sys.stdin)$1)" 2>/dev/null || echo ""; }
read_su_pw() { $KUBECTL -n pglord get secret ${CL}-credentials -o jsonpath='{.data.superuser-password}' | base64 -d; }

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
$KUBECTL -n pglord port-forward svc/pglord-server 18099:80 >/dev/null 2>&1 &
sleep 2

echo "==> rejecting an invalid pooler block"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"$CL\",\"replicas\":2,\"pooler\":{\"enabled\":true,\"replicas\":0}}")
[ "$code" = "422" ] || { echo "FAIL: pooler replicas=0 returned $code, expected 422"; exit 1; }
echo "OK invalid pooler rejected"

echo "==> creating cluster '$CL' (replicas=2, pooler on, init script)"
$KUBECTL -n pglord create configmap ${CL}-init \
  --from-literal=01-app.sql="CREATE ROLE app_rw LOGIN PASSWORD 'app-pw-1'; CREATE DATABASE appdb OWNER app_rw;" >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"$CL\",\"replicas\":2,\"init\":{\"scripts_config_map\":\"${CL}-init\"},\"pooler\":{\"enabled\":true,\"default_pool_size\":2}}")
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field "['ready_replicas']")" = "2" ] && break
  sleep 2
done
[ "$(field "['ready_replicas']")" = "2" ] || { echo "FAIL: never reached 2/2"; exit 1; }
SU_PW=$(read_su_pw)
echo "OK cluster ready"

echo "==> pooler deployment comes up and detail reports it"
$KUBECTL -n pglord rollout status deploy/${CL}-pooler --timeout=120s >/dev/null \
  || { echo "FAIL: pooler deployment never became ready"; exit 1; }
for _ in $(seq 1 30); do
  [ "$(field "['pooler']['ready_replicas']")" = "1" ] && break
  sleep 2
done
[ "$(field "['pooler']['ready_replicas']")" = "1" ] || { echo "FAIL: detail never reported pooler ready"; exit 1; }
[ "$(field "['pooler']['service']")" = "${CL}-pool" ] || { echo "FAIL: detail pool service name wrong"; exit 1; }
echo "OK pooler up, reported on the detail"

echo "==> superuser connects through the pool (auth_query path)"
# The reconciler ensures the auth role level-triggered; give it a few passes.
for _ in $(seq 1 45); do
  [ "$(pool_psql "$SU_PW" postgres postgres 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(pool_psql "$SU_PW" postgres postgres 'SELECT 1')" = "1" ] \
  || { echo "FAIL: superuser cannot connect via ${CL}-pool"; exit 1; }
echo "OK pooled connection works"

echo "==> init-script role connects through the pool with its own password"
for _ in $(seq 1 30); do
  [ "$(pool_psql app-pw-1 app_rw appdb 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(pool_psql app-pw-1 app_rw appdb 'SELECT 1')" = "1" ] \
  || { echo "FAIL: app_rw cannot connect via the pool"; exit 1; }
[ "$(pool_psql wrong-pw app_rw appdb 'SELECT 1')" = "DENIED" ] \
  || { echo "FAIL: wrong password accepted through the pool"; exit 1; }
echo "OK auth_query authenticates app roles, wrong password rejected"

echo "==> transaction pooling shares server connections (pool_size=2)"
prim=$(field "['primary']")
# Collect the client PIDs: a bare `wait` would also wait for the port-forward
# background job, which never exits.
client_pids=()
for _ in $(seq 1 6); do
  $KUBECTL -n pglord exec ${CL}-0 -- env PGPASSWORD="$SU_PW" \
    psql -h ${CL}-pool -U postgres -tAc "SELECT pg_sleep(6)" >/dev/null 2>&1 &
  client_pids+=($!)
done
sleep 3
# Network backends as postgres on the primary = pooled server connections
# (the local-socket harness connections have no client_addr).
server_conns=$(psql_on "$prim" "SELECT count(*) FROM pg_stat_activity WHERE usename='postgres' AND client_addr IS NOT NULL")
wait "${client_pids[@]}" 2>/dev/null || true
[ -n "$server_conns" ] || { echo "FAIL: could not read pg_stat_activity"; exit 1; }
[ "$server_conns" -ge 1 ] && [ "$server_conns" -le 2 ] \
  || { echo "FAIL: 6 pooled clients used $server_conns server connections (expected 1..2)"; exit 1; }
echo "OK 6 concurrent clients shared $server_conns server connections"

echo "==> knob change rolls only pooler pods"
inst_before0=$(restarts_of ${CL}-0); inst_before1=$(restarts_of ${CL}-1)
pooler_pod_before=$($KUBECTL -n pglord get pods -l pglord.dev/role=pooler -o jsonpath='{.items[0].metadata.name}')
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' \
  -d '{"pooler":{"enabled":true,"default_pool_size":5}}')
[ "$code" = "200" ] || { echo "FAIL: pooler patch returned $code"; exit 1; }
for _ in $(seq 1 60); do
  now=$($KUBECTL -n pglord get pods -l pglord.dev/role=pooler -o jsonpath='{.items[*].metadata.name}')
  case " $now " in
    *" $pooler_pod_before "*) sleep 2 ;;
    *) break ;;
  esac
done
now=$($KUBECTL -n pglord get pods -l pglord.dev/role=pooler -o jsonpath='{.items[*].metadata.name}')
case " $now " in
  *" $pooler_pod_before "*) echo "FAIL: pooler pod did not roll on a knob change"; exit 1 ;;
esac
$KUBECTL -n pglord rollout status deploy/${CL}-pooler --timeout=120s >/dev/null
[ "$(restarts_of ${CL}-0)" = "$inst_before0" ] && [ "$(restarts_of ${CL}-1)" = "$inst_before1" ] \
  || { echo "FAIL: postgres instances restarted for a pooler-only change"; exit 1; }
echo "OK pooler rolled, instances untouched"

echo "==> failover under the pool"
prim=$(field "['primary']")
$KUBECTL -n pglord delete pod "$prim" >/dev/null
ok=""
for _ in $(seq 1 120); do
  if [ "$(pool_psql "$SU_PW" postgres postgres 'SELECT 1')" = "1" ]; then ok=yes; break; fi
  sleep 2
done
[ "$ok" = "yes" ] || { echo "FAIL: pool never recovered after primary loss"; exit 1; }
echo "OK pooled access recovered after failover"

echo "==> credentials rotation rotates the pooler credential too"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CL/rotate-credentials")
[ "$code" = "202" ] || { echo "FAIL: rotate returned $code"; exit 1; }
ok=""
for _ in $(seq 1 120); do
  SU_PW=$(read_su_pw)
  if [ "$(pool_psql "$SU_PW" postgres postgres 'SELECT 1')" = "1" ]; then ok=yes; break; fi
  sleep 2
done
[ "$ok" = "yes" ] || { echo "FAIL: pool broken after rotation"; exit 1; }
echo "OK pool works with rotated credentials"

echo "==> disabling the pooler deletes its objects"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CL" \
  -H 'content-type: application/json' -d '{"pooler":{"enabled":false}}')
[ "$code" = "200" ] || { echo "FAIL: disable returned $code"; exit 1; }
gone=""
for _ in $(seq 1 60); do
  d=$($KUBECTL -n pglord get deploy ${CL}-pooler --ignore-not-found -o name)
  s=$($KUBECTL -n pglord get svc ${CL}-pool --ignore-not-found -o name)
  c=$($KUBECTL -n pglord get secret ${CL}-pooler-config --ignore-not-found -o name)
  [ -z "$d$s$c" ] && { gone=yes; break; }
  sleep 2
done
[ "$gone" = "yes" ] || { echo "FAIL: pooler objects survived disable"; exit 1; }
[ "$(psql_on "$(field "['primary']")" 'SELECT 1')" = "1" ] || { echo "FAIL: cluster unhealthy after disable"; exit 1; }
echo "OK objects removed, cluster healthy"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/$CL" >/dev/null
echo "pooler e2e passed"
