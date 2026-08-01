#!/usr/bin/env bash
# Config management e2e: a reload-safe setting applies without restarting, a
# restart-required setting is reported as pending and applied by the rolling
# restart, and the data survives it.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18098

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/cfg" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

psql_on() { $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }
restarts_of() { $KUBECTL -n pglord get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?"; }
detail() { curl -s "$API/api/v1/clusters/cfg"; }
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
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18098:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster 'cfg' (replicas=2)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d '{"name":"cfg","replicas":2}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field "['ready_replicas']")" = "2" ] && break
  sleep 2
done
[ "$(field "['ready_replicas']")" = "2" ] || { echo "FAIL: never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret cfg-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
psql_on cfg-0 "CREATE TABLE cfg_e2e(x int); INSERT INTO cfg_e2e VALUES (5);" >/dev/null
echo "OK cluster ready"

echo "==> rejecting bad settings"
for body in '{"parameters":{"wrk_mem":"16MB"}}' '{"parameters":{"wal_level":"minimal"}}' '{"parameters":{"work_mem":"lots"}}'; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" -H 'content-type: application/json' -d "$body")
  [ "$code" = "422" ] || { echo "FAIL: $body returned $code, expected 422"; exit 1; }
done
echo "OK unknown, reserved and malformed settings rejected"

# The pg_hba section runs before the restart-required steps on purpose: rules
# apply with a reload, and a freshly created cluster has stable pod IPs and a
# known primary (cfg-0). After a roll, pod churn leaves stale headless-service
# DNS ("No route to host") and the orchestrator can depose the just-restarted
# primary, which makes TCP reachability assertions ambiguous.
echo "==> pg_hba: invalid rule rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"pg_hba":[{"conn":"host","database":"all","user":"postgres","address":"not-a-cidr","method":"reject"}]}')
[ "$code" = "422" ] || { echo "FAIL: bad hba rule returned $code, expected 422"; exit 1; }
echo "OK invalid address rejected"

echo "==> pg_hba: reject rule blocks network clients, replication survives"
# Network path: exec on cfg-1, connect to cfg-0 over TCP (local socket access
# is peer/managed and must keep working regardless of user rules).
net_psql() { $KUBECTL -n pglord exec cfg-1 -- env PGPASSWORD="$SU_PW" psql -h cfg-0.cfg-pods -U postgres -tAc "$1" 2>/dev/null || echo "DENIED"; }
# Retried like the other stateful checks: a single kubectl exec can hiccup.
for _ in $(seq 1 15); do
  [ "$(net_psql 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(net_psql 'SELECT 1')" = "1" ] || { echo "FAIL: network access broken before any rule"; exit 1; }
before0=$(restarts_of cfg-0); before1=$(restarts_of cfg-1)
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"pg_hba":[{"conn":"host","database":"all","user":"postgres","address":"0.0.0.0/0","method":"reject"}]}')
[ "$code" = "200" ] || { echo "FAIL: hba patch returned $code"; exit 1; }
for _ in $(seq 1 30); do
  [ "$(net_psql 'SELECT 1')" = "DENIED" ] && break
  sleep 2
done
[ "$(net_psql 'SELECT 1')" = "DENIED" ] || { echo "FAIL: reject rule not enforced"; exit 1; }
[ "$(psql_on cfg-0 'SELECT 1')" = "1" ] || { echo "FAIL: local socket access broken by user rule"; exit 1; }
# pg_stat_replication lives on the primary; ask the record which pod that is.
hba_primary=$(field "['primary']")
[ "$(psql_on "$hba_primary" 'SELECT count(*) FROM pg_stat_replication')" = "1" ] \
  || { echo "FAIL: replication lost after hba change"; exit 1; }
[ "$(restarts_of cfg-0)" = "$before0" ] && [ "$(restarts_of cfg-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a reload-safe hba change"; exit 1; }
echo "OK network rejected, socket and replication intact, no restarts"

echo "==> pg_hba: clearing rules restores access"
curl -s -o /dev/null -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"pg_hba":[]}'
for _ in $(seq 1 30); do
  [ "$(net_psql 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(net_psql 'SELECT 1')" = "1" ] || { echo "FAIL: access not restored after clearing rules"; exit 1; }
echo "OK rules cleared"

echo "==> applying a reload-safe setting (work_mem)"
before0=$(restarts_of cfg-0); before1=$(restarts_of cfg-1)
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"parameters":{"work_mem":"8MB"}}')
[ "$code" = "200" ] || { echo "FAIL: patch returned $code"; exit 1; }
for _ in $(seq 1 30); do
  v0=$(psql_on cfg-0 "SHOW work_mem"); v1=$(psql_on cfg-1 "SHOW work_mem")
  [ "$v0" = "8MB" ] && [ "$v1" = "8MB" ] && break
  sleep 2
done
[ "$v0" = "8MB" ] && [ "$v1" = "8MB" ] || { echo "FAIL: work_mem not applied (cfg-0='$v0' cfg-1='$v1')"; exit 1; }
[ "$(restarts_of cfg-0)" = "$before0" ] && [ "$(restarts_of cfg-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a reload-safe setting"; exit 1; }
[ "$(field "['pending_restart']")" = "False" ] || { echo "FAIL: pending_restart set after a reload-safe change"; exit 1; }
echo "OK applied live on both instances, no restarts"

echo "==> applying a restart-required setting (max_connections)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"parameters":{"work_mem":"8MB","max_connections":"150"}}')
[ "$code" = "200" ] || { echo "FAIL: patch returned $code"; exit 1; }
# Every instance must have staged the setting: the agents poll the spec on
# their own 5s clocks, and restarting an instance before its agent wrote the
# new file would reboot it into the old value.
all_pending() { detail | python3 -c "import sys,json; d=json.load(sys.stdin); print(bool(d['instances']) and all(i['pending_restart'] for i in d['instances']))" 2>/dev/null || echo ""; }
for _ in $(seq 1 30); do
  [ "$(all_pending)" = "True" ] && break
  sleep 2
done
[ "$(all_pending)" = "True" ] || { echo "FAIL: pending_restart not reported on every instance"; exit 1; }
[ "$(psql_on cfg-0 "SHOW max_connections")" = "100" ] || { echo "FAIL: max_connections changed without a restart"; exit 1; }
echo "OK reported as pending, not yet in effect"

echo "==> rolling restart"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/cfg/restart")
[ "$code" = "202" ] || { echo "FAIL: restart returned $code"; exit 1; }
for _ in $(seq 1 120); do
  v0=$(psql_on cfg-0 "SHOW max_connections"); v1=$(psql_on cfg-1 "SHOW max_connections")
  [ "$v0" = "150" ] && [ "$v1" = "150" ] && break
  sleep 2
done
[ "$v0" = "150" ] && [ "$v1" = "150" ] || { echo "FAIL: max_connections not applied (cfg-0='$v0' cfg-1='$v1')"; exit 1; }
for _ in $(seq 1 30); do
  [ "$(field "['pending_restart']")" = "False" ] && break
  sleep 2
done
[ "$(field "['pending_restart']")" = "False" ] || { echo "FAIL: pending_restart still set after restart"; exit 1; }
[ "$(psql_on cfg-0 "SELECT x FROM cfg_e2e")" = "5" ] || { echo "FAIL: data lost across restart"; exit 1; }
echo "OK setting applied cluster-wide, pending cleared, data intact"

echo "==> clearing settings returns to defaults"
curl -s -o /dev/null -X PATCH "$API/api/v1/clusters/cfg" -H 'content-type: application/json' -d '{"parameters":{}}'
for _ in $(seq 1 30); do
  [ "$(psql_on cfg-0 "SHOW work_mem")" = "4MB" ] && break
  sleep 2
done
[ "$(psql_on cfg-0 "SHOW work_mem")" = "4MB" ] || { echo "FAIL: work_mem did not return to default"; exit 1; }
echo "OK settings cleared"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/cfg" >/dev/null
echo "config e2e passed"
