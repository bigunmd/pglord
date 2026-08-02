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
# The rm above fails silently while any pod still runs the image (leftover
# clusters do this), and kubelet then keeps serving the stale build. Verify
# the stand really runs what was just built before trusting any result.
for img in pglord-server:dev pglord-agent:dev; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img (have $have, want $want) — delete leftover clusters and retry"; exit 1; }
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

echo "==> synchronous replication: validation guard rails"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"synchronous":true,"parameters":{"synchronous_commit":"off"}}')
[ "$code" = "422" ] || { echo "FAIL: sync + synchronous_commit=off returned $code"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"parameters":{"synchronous_standby_names":"ANY 1 (x)"}}')
[ "$code" = "422" ] || { echo "FAIL: reserved synchronous_standby_names returned $code"; exit 1; }
echo "OK rejected as 422"

echo "==> enabling synchronous replication"
before0=$(restarts_of cfg-0); before1=$(restarts_of cfg-1)
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"synchronous":true}')
[ "$code" = "200" ] || { echo "FAIL: enable sync returned $code"; exit 1; }
# The rolling restart above may have ended in a failover, and the primary
# record can lag the actual roles briefly — ask postgres itself who is
# primary, and poll names + sync_state together (the walreceiver may still be
# reconnecting when the setting lands).
prim=""; stand=""; names=""; state=""
for _ in $(seq 1 30); do
  if [ "$(psql_on cfg-0 "SELECT pg_is_in_recovery()")" = "f" ]; then
    prim=cfg-0; stand=cfg-1
  elif [ "$(psql_on cfg-1 "SELECT pg_is_in_recovery()")" = "f" ]; then
    prim=cfg-1; stand=cfg-0
  else
    sleep 2; continue
  fi
  names=$(psql_on "$prim" "SHOW synchronous_standby_names")
  state=$(psql_on "$prim" "SELECT sync_state FROM pg_stat_replication LIMIT 1")
  [ "$names" = "ANY 1 (\"$stand\")" ] && [ "$state" = "quorum" ] && break
  sleep 2
done
if [ "$names" != "ANY 1 (\"$stand\")" ] || [ "$state" != "quorum" ]; then
  echo "DIAG prim=$prim stand=$stand names='$names' state='$state'"
  echo "DIAG recovery: cfg-0=$(psql_on cfg-0 'SELECT pg_is_in_recovery()') cfg-1=$(psql_on cfg-1 'SELECT pg_is_in_recovery()')"
  echo "DIAG walreceiver on $stand: $(psql_on "$stand" 'SELECT status, sender_host FROM pg_stat_wal_receiver')"
  echo "DIAG replication on $prim: $(psql_on "$prim" 'SELECT application_name, state, sync_state FROM pg_stat_replication')"
  echo "DIAG pods:"; $KUBECTL -n pglord get pods -o wide | grep '^cfg-' || true
  echo "DIAG $stand logs:"; $KUBECTL -n pglord logs "$stand" --tail=20 || true
  echo "FAIL: sync not converged (names='$names' state='$state')"
  exit 1
fi
# A commit must complete while the quorum standby is connected.
psql_on "$prim" "CREATE TABLE IF NOT EXISTS sync_e2e(x int)" >/dev/null
psql_on "$prim" "INSERT INTO sync_e2e VALUES (1)" >/dev/null
[ "$(psql_on "$prim" "SELECT count(*) FROM sync_e2e")" = "1" ] \
  || { echo "FAIL: sync write did not commit"; exit 1; }
[ "$(field "['synchronous']")" = "True" ] || { echo "FAIL: detail synchronous not true"; exit 1; }
[ "$(field "['sync_degraded']")" = "False" ] || { echo "FAIL: sync_degraded set with a healthy standby"; exit 1; }
[ "$(restarts_of cfg-0)" = "$before0" ] && [ "$(restarts_of cfg-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a reload-safe change"; exit 1; }
echo "OK quorum sync active, write committed, no restarts"

echo "==> scaling a sync cluster to 1 replica is rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"replicas":1}')
[ "$code" = "422" ] || { echo "FAIL: scale-to-1 with sync on returned $code"; exit 1; }
echo "OK rejected as 422"

echo "==> disabling synchronous replication"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"synchronous":false}')
[ "$code" = "200" ] || { echo "FAIL: disable sync returned $code"; exit 1; }
for _ in $(seq 1 30); do
  names=$(psql_on "$prim" "SHOW synchronous_standby_names")
  [ "$names" = "" ] && break
  sleep 2
done
[ "$names" = "" ] || { echo "FAIL: synchronous_standby_names not cleared ('$names')"; exit 1; }
echo "OK cleared"

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

echo "==> hot-standby clamp: rebuild survives a lowered restart GUC"
# Precondition from the sections above: max_connections was rolled to 150,
# then the spec was cleared — so the primary keeps 150 live (pending_restart)
# while the spec says default. Rebuilding a standby now used to crashloop with
# "recovery aborted because of insufficient parameter settings"; the agent's
# control-file clamp must keep it startable.
prim=""; stand=""
for _ in $(seq 1 30); do
  if [ "$(psql_on cfg-0 "SELECT pg_is_in_recovery()")" = "f" ]; then
    prim=cfg-0; stand=cfg-1; break
  elif [ "$(psql_on cfg-1 "SELECT pg_is_in_recovery()")" = "f" ]; then
    prim=cfg-1; stand=cfg-0; break
  fi
  sleep 2
done
[ -n "$prim" ] || { echo "FAIL: no primary found before rebuild"; exit 1; }
[ "$(psql_on "$prim" "SHOW max_connections")" = "150" ] \
  || { echo "FAIL: precondition lost, primary not live at 150"; exit 1; }
# Wiping PGDATA (not the PVC) forces the same basebackup rebuild. Deleting
# the PVC would too on a real storage driver, but minikube's hostpath
# provisioner keys directories by PVC name: the old PV's deletion races the
# replacement's creation on the same path and leaves a root-owned mount that
# fsGroup cannot fix (fsGroup is ignored for hostPath volumes).
$KUBECTL -n pglord exec "$stand" -- rm -rf /var/lib/postgresql/data/pgdata
$KUBECTL -n pglord delete pod "$stand" >/dev/null
for _ in $(seq 1 120); do
  [ "$(psql_on "$stand" 'SELECT 1')" = "1" ] && break
  sleep 2
done
if [ "$(psql_on "$stand" 'SELECT 1')" != "1" ]; then
  echo "DIAG pods:"; $KUBECTL -n pglord get pods -o wide | grep '^cfg-' || true
  echo "DIAG $stand logs:"; $KUBECTL -n pglord logs "$stand" --tail=30 || true
  echo "FAIL: rebuilt standby never came back"
  exit 1
fi
# The clamp must hold the standby at the primary's live value, not the spec's.
[ "$(psql_on "$stand" "SHOW max_connections")" = "150" ] \
  || { echo "FAIL: standby not clamped to the primary's live max_connections"; exit 1; }
[ "$(psql_on "$stand" "SELECT pg_is_in_recovery()")" = "t" ] \
  || { echo "FAIL: rebuilt instance is not a standby"; exit 1; }
echo "OK rebuilt standby healthy while primary still runs the higher value"

# Rolls converge everyone back to the spec (default 100). One roll is not
# always enough: the rebuilt standby restarts before the primary, while 150
# is still live, so it re-clamps; only after the primary drops does the
# watch loop clear the clamp, and the next roll lands the standby on the
# spec value. Poll values directly rather than guessing roll boundaries.
converged=""
for roll in 1 2 3; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/cfg/restart")
  [ "$code" = "202" ] || { echo "FAIL: restart #$roll returned $code"; exit 1; }
  for _ in $(seq 1 75); do
    v0=$(psql_on cfg-0 "SHOW max_connections"); v1=$(psql_on cfg-1 "SHOW max_connections")
    [ "$v0" = "100" ] && [ "$v1" = "100" ] && { converged=yes; break; }
    sleep 2
  done
  [ "$converged" = "yes" ] && break
  # Not yet: give the watch loop a couple of 5s passes to clear the clamp
  # before rolling again.
  sleep 12
done
[ "$converged" = "yes" ] \
  || { echo "FAIL: cluster did not converge to the default (cfg-0='$v0' cfg-1='$v1')"; exit 1; }
echo "OK rolls converged every instance back to the spec value"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/cfg" >/dev/null
echo "config e2e passed"
