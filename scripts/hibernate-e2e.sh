#!/usr/bin/env bash
# Hibernation + fencing e2e: fence a standby (postgres held down, pod up),
# unfence (rejoins), hibernate (0 pods, PVCs kept), guards, wake (data intact).
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18096

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

psql_on() { # pod sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""
}
detail() { curl -s "$API/api/v1/clusters/demo"; }
jsonq() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null || echo ""; }

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
$KUBECTL -n pglord port-forward svc/pglord-server 18096:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster 'demo' (replicas=2)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d '{"name":"demo","replicas":2}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
ready=""
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret demo-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
psql_on demo-0 "CREATE TABLE hib(x int); INSERT INTO hib VALUES (42);" >/dev/null
sleep 3
echo "OK cluster ready"

# The record can name either pod (post-roll deposals happen on the stand);
# fence the pod that is NOT the recorded primary.
PRIMARY=$(detail | jsonq "d['primary']")
if [ "$PRIMARY" = "demo-0" ]; then STANDBY=demo-1; else STANDBY=demo-0; fi
echo "==> primary=$PRIMARY standby=$STANDBY"

echo "==> fencing the primary is refused"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/instances/$PRIMARY/fence")
[ "$code" = "409" ] || { echo "FAIL: fence primary returned $code, expected 409"; exit 1; }
body=$(curl -s -X POST "$API/api/v1/clusters/demo/instances/$PRIMARY/fence")
echo "$body" | grep -q cannot_fence_primary || { echo "FAIL: wrong code in $body"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/instances/demo-9/fence")
[ "$code" = "422" ] || { echo "FAIL: bad pod returned $code, expected 422"; exit 1; }
echo "OK fence validation"

echo "==> fencing $STANDBY"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/instances/$STANDBY/fence")
[ "$code" = "202" ] || { echo "FAIL: fence returned $code"; exit 1; }
role=""
for _ in $(seq 1 60); do
  role=$(detail | jsonq "[i['role'] for i in d['instances'] if i['pod']=='$STANDBY'][0]")
  [ "$role" = "fenced" ] && break
  sleep 2
done
[ "$role" = "fenced" ] || { echo "FAIL: $STANDBY never reported fenced (role=$role)"; exit 1; }
# postgres must actually be down in the pod while the pod itself stays up.
pg_up=""
for _ in $(seq 1 30); do
  pg_up=$($KUBECTL -n pglord exec "$STANDBY" -- pgrep -x postgres 2>/dev/null | head -1 || echo "")
  [ -z "$pg_up" ] && break
  sleep 2
done
[ -z "$pg_up" ] || { echo "FAIL: postgres still running in fenced $STANDBY"; exit 1; }
phase=$($KUBECTL -n pglord get pod "$STANDBY" -o jsonpath='{.status.phase}')
[ "$phase" = "Running" ] || { echo "FAIL: fenced pod phase=$phase, expected Running"; exit 1; }
val=$(psql_on "$PRIMARY" "SELECT x FROM hib")
[ "$val" = "42" ] || { echo "FAIL: cluster stopped serving while standby fenced"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/instances/$STANDBY/fence")
[ "$code" = "409" ] || { echo "FAIL: double fence returned $code, expected 409"; exit 1; }
echo "OK $STANDBY fenced: postgres down, pod up, cluster serving"

echo "==> unfencing $STANDBY"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/instances/$STANDBY/unfence")
[ "$code" = "202" ] || { echo "FAIL: unfence returned $code"; exit 1; }
streaming=""
for _ in $(seq 1 90); do
  streaming=$(psql_on "$PRIMARY" "SELECT count(*) FROM pg_stat_replication WHERE state='streaming'")
  [ "$streaming" = "1" ] && break
  sleep 2
done
[ "$streaming" = "1" ] || { echo "FAIL: $STANDBY never rejoined as a streaming standby"; exit 1; }
echo "OK $STANDBY rejoined"

echo "==> hibernating"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/hibernate")
[ "$code" = "202" ] || { echo "FAIL: hibernate returned $code"; exit 1; }
pods=""
for _ in $(seq 1 90); do
  pods=$($KUBECTL -n pglord get pods -l app.kubernetes.io/instance=demo --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "$pods" = "0" ] && break
  sleep 2
done
[ "$pods" = "0" ] || { echo "FAIL: pods never reached 0 (have $pods)"; exit 1; }
# grep -c prints the 0 itself (while exiting 1), so the fallback must add
# no output of its own or the count comes out as "0\n0".
pvcs=$($KUBECTL -n pglord get pvc --no-headers 2>/dev/null | grep -c 'demo-' || true)
[ "$pvcs" -ge 2 ] || { echo "FAIL: expected >=2 PVCs retained, have $pvcs"; exit 1; }
hib=$(detail | jsonq "d['hibernated']")
[ "$hib" = "True" ] || { echo "FAIL: detail hibernated=$hib"; exit 1; }
echo "OK hibernated: 0 pods, $pvcs PVCs retained"

echo "==> mutating operations answer 409 hibernated"
for op in "PATCH /api/v1/clusters/demo {\"replicas\":3}" \
          "POST /api/v1/clusters/demo/restart" \
          "POST /api/v1/clusters/demo/switchover {\"target\":\"demo-1\"}" \
          "POST /api/v1/clusters/demo/instances/demo-1/fence"; do
  method=$(echo "$op" | cut -d' ' -f1)
  path=$(echo "$op" | cut -d' ' -f2)
  data=$(echo "$op" | cut -d' ' -f3- -s | grep -v "^$path$" || true)
  if [ -n "$data" ] && [ "$data" != "$path" ]; then
    resp=$(curl -s -w '\n%{http_code}' -X "$method" "$API$path" -H 'content-type: application/json' -d "$data")
  else
    resp=$(curl -s -w '\n%{http_code}' -X "$method" "$API$path")
  fi
  code=$(echo "$resp" | tail -1)
  body=$(echo "$resp" | head -n -1)
  [ "$code" = "409" ] || { echo "FAIL: $method $path returned $code, expected 409"; exit 1; }
  echo "$body" | grep -q '"hibernated"' || { echo "FAIL: $method $path wrong code: $body"; exit 1; }
done
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/hibernate")
[ "$code" = "409" ] || { echo "FAIL: double hibernate returned $code, expected 409"; exit 1; }
echo "OK guards hold"

echo "==> waking"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/demo/wake")
[ "$code" = "202" ] || { echo "FAIL: wake returned $code"; exit 1; }
ready=""
for _ in $(seq 1 180); do
  ready=$(detail | jsonq "d['ready_replicas']")
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never woke to 2/2"; exit 1; }
# Post-wake the record may name either pod; read through the current primary.
PRIMARY=$(detail | jsonq "d['primary']")
val=""
for _ in $(seq 1 30); do
  val=$(psql_on "$PRIMARY" "SELECT x FROM hib")
  [ "$val" = "42" ] && break
  sleep 2
done
[ "$val" = "42" ] || { echo "FAIL: marker row lost across hibernation (got '$val')"; exit 1; }
echo "OK woke with data intact"

echo "==> deleting cluster garbage-collects storage"
curl -s -X DELETE "$API/api/v1/clusters/demo" >/dev/null
gone=""
for _ in $(seq 1 90); do
  left=$($KUBECTL -n pglord get pvc --no-headers 2>/dev/null | grep -c 'demo-' || true)
  [ "$left" = "0" ] && gone=yes && break
  sleep 2
done
[ "$gone" = "yes" ] || { echo "FAIL: PVCs survived cluster deletion"; exit 1; }
echo "OK PVCs deleted with the cluster"

echo "PASS: hibernation + fencing e2e"
