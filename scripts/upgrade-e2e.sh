#!/usr/bin/env bash
# Major version upgrade e2e: PostgreSQL 16 -> 17 through the blue-green flow.
# Two scenarios against one cluster: cut over then roll back, then cut over
# again and finalize.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18102
CLUSTER=updemo

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/$CLUSTER" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

detail() { curl -s "$API/api/v1/clusters/$CLUSTER"; }

# Read one field out of the cluster detail; "" when absent, so callers can
# compare without worrying about nulls.
field() { # jsonpath-ish key
  detail | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('$1',''))" 2>/dev/null || echo ""
}

upgrade_field() { # key
  detail | python3 -c "import sys,json; u=(json.load(sys.stdin).get('upgrade') or {}); print(u.get('$1',''))" 2>/dev/null || echo ""
}

# psql against a specific pod's local socket. Errors surface as "" so callers
# assert on the value rather than on the exit code.
psql_on() { # pod db sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" \
    psql -U postgres -d "$2" -tAc "$3" 2>/dev/null || echo ""
}

# Same, but keeps stderr: for asserting that a write is refused.
psql_err() { # pod db sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" \
    psql -U postgres -d "$2" -tAc "$3" 2>&1 || true
}

primary_pod() { field primary; }

echo "==> building + loading images (server, agent PG16, agent PG17)"
make image >/dev/null
docker build -q -f deploy/Dockerfile.agent --build-arg PG_MAJOR=16 -t pglord-agent:dev16 . >/dev/null
docker build -q -f deploy/Dockerfile.agent --build-arg PG_MAJOR=17 -t pglord-agent:dev17 . >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
for img in pglord-server:dev pglord-agent:dev16 pglord-agent:dev17; do
  minikube -p pglord image rm "docker.io/library/$img" >/dev/null 2>&1 || true
  minikube -p pglord image load "$img"
done
# A failed rm is silent while any pod still uses the image, and kubelet then
# serves the stale build — verify the stand runs what was just built.
for img in pglord-server:dev pglord-agent:dev16 pglord-agent:dev17; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img (have $have, want $want)"; exit 1; }
done
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18102:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating '$CLUSTER' on PostgreSQL 16 (replicas=2, appdb, pgcrypto)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\": \"$CLUSTER\", \"image\": \"pglord-agent:dev16\", \"replicas\": 2,
    \"databases\": [{\"name\": \"appdb\"}],
    \"extensions\": [{\"name\": \"pgcrypto\", \"database\": \"appdb\"}]
  }")
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
ready=""
for _ in $(seq 1 180); do
  ready=$(field ready_replicas)
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret "$CLUSTER-credentials" -o jsonpath='{.data.superuser-password}' | base64 -d)
BLUE_PRIMARY=$(primary_pod)
echo "OK cluster ready (primary $BLUE_PRIMARY)"

echo "==> asserting the running major and wal_level"
ver=$(psql_on "$BLUE_PRIMARY" postgres "SHOW server_version_num")
case "$ver" in 16*) ;; *) echo "FAIL: expected PostgreSQL 16, got server_version_num=$ver"; exit 1;; esac
lvl=$(psql_on "$BLUE_PRIMARY" postgres "SHOW wal_level")
[ "$lvl" = "logical" ] || { echo "FAIL: wal_level is '$lvl', expected logical"; exit 1; }
echo "OK PG16 with wal_level=logical"

echo "==> seeding data (100 rows, a sequence, an extension)"
psql_on "$BLUE_PRIMARY" appdb \
  "CREATE TABLE t(id bigserial primary key, v text); \
   INSERT INTO t(v) SELECT 'row-' || g FROM generate_series(1,100) g;" >/dev/null
seeded=$(psql_on "$BLUE_PRIMARY" appdb "SELECT count(*) FROM t")
[ "$seeded" = "100" ] || { echo "FAIL: expected 100 seeded rows, got '$seeded'"; exit 1; }
have_pgcrypto=$(psql_on "$BLUE_PRIMARY" appdb "SELECT count(*) FROM pg_extension WHERE extname='pgcrypto'")
[ "$have_pgcrypto" = "1" ] || { echo "FAIL: pgcrypto not installed in appdb"; exit 1; }
echo "OK seeded"

start_upgrade() {
  curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade" \
    -H 'content-type: application/json' -d '{"target_image":"pglord-agent:dev17"}'
}

wait_ready_to_cutover() {
  local ready=""
  for _ in $(seq 1 150); do
    ready=$(upgrade_field ready_to_cutover)
    [ "$ready" = "True" ] && return 0
    sleep 2
  done
  echo "FAIL: never became ready to cut over (last error: $(upgrade_field error))"
  return 1
}

wait_phase() { # expected
  local phase=""
  for _ in $(seq 1 60); do
    phase=$(upgrade_field phase)
    [ "$phase" = "$1" ] && return 0
    sleep 2
  done
  echo "FAIL: upgrade phase is '$phase', expected '$1'"
  return 1
}

wait_no_upgrade() {
  for _ in $(seq 1 90); do
    [ -z "$(upgrade_field phase)" ] && return 0
    sleep 2
  done
  echo "FAIL: upgrade block never cleared"
  return 1
}

# ---------------------------------------------------------------- scenario A
echo "==> scenario A: start, cut over, roll back"
code=$(start_upgrade)
[ "$code" = "202" ] || { echo "FAIL: upgrade start returned $code"; exit 1; }
GEN_A=$(upgrade_field next_generation)
[ -n "$GEN_A" ] || { echo "FAIL: no next_generation reported"; exit 1; }
echo "OK upgrade started (generation $GEN_A)"

echo "==> day-2 operations are refused while upgrading"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/$CLUSTER" \
  -H 'content-type: application/json' -d '{"replicas":3}')
[ "$code" = "409" ] || { echo "FAIL: PATCH during upgrade returned $code, expected 409"; exit 1; }
reason=$(curl -s -X PATCH "$API/api/v1/clusters/$CLUSTER" -H 'content-type: application/json' \
  -d '{"replicas":3}' | python3 -c "import sys,json; print(json.load(sys.stdin)['code'])")
[ "$reason" = "upgrade_in_progress" ] || { echo "FAIL: code was '$reason'"; exit 1; }
echo "OK day-2 blocked with upgrade_in_progress"

wait_ready_to_cutover || exit 1
echo "OK ready to cut over"

# Rows written right up to the cutover must survive it.
psql_on "$BLUE_PRIMARY" appdb "INSERT INTO t(v) SELECT 'late-' || g FROM generate_series(1,10) g;" >/dev/null
before_cutover=$(psql_on "$BLUE_PRIMARY" appdb "SELECT count(*) FROM t")
[ "$before_cutover" = "110" ] || { echo "FAIL: expected 110 rows before cutover, got '$before_cutover'"; exit 1; }

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade/cutover")
[ "$code" = "202" ] || { echo "FAIL: cutover returned $code"; exit 1; }
wait_phase soaking || exit 1
GREEN_PRIMARY="$CLUSTER-$GEN_A-0"
[ "$(primary_pod)" = "$GREEN_PRIMARY" ] || { echo "FAIL: primary is $(primary_pod), expected $GREEN_PRIMARY"; exit 1; }
echo "OK cut over to $GREEN_PRIMARY"

echo "==> the new instances run PostgreSQL 17 and hold every row"
ver=$(psql_on "$GREEN_PRIMARY" postgres "SHOW server_version_num")
case "$ver" in 17*) ;; *) echo "FAIL: expected PostgreSQL 17 after cutover, got '$ver'"; exit 1;; esac
after=$(psql_on "$GREEN_PRIMARY" appdb "SELECT count(*) FROM t")
[ "$after" = "110" ] || { echo "FAIL: expected 110 rows after cutover, got '$after'"; exit 1; }
echo "OK PG17 with all 110 rows"

echo "==> the old primary is read-only while soaking"
err=$(psql_err "$BLUE_PRIMARY" appdb "INSERT INTO t(v) VALUES ('should-fail')")
case "$err" in *read-only*) ;; *) echo "FAIL: old primary accepted a write: $err"; exit 1;; esac
echo "OK old primary fenced"

echo "==> writing on the new version, then rolling back"
psql_on "$GREEN_PRIMARY" appdb "INSERT INTO t(v) VALUES ('post-cutover')" >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade/rollback")
[ "$code" = "202" ] || { echo "FAIL: rollback returned $code"; exit 1; }
wait_no_upgrade || exit 1
[ "$(primary_pod)" = "$BLUE_PRIMARY" ] || { echo "FAIL: primary is $(primary_pod), expected $BLUE_PRIMARY after rollback"; exit 1; }
psql_on "$BLUE_PRIMARY" appdb "INSERT INTO t(v) VALUES ('after-rollback')" >/dev/null
rolled=$(psql_on "$BLUE_PRIMARY" appdb "SELECT count(*) FROM t")
[ "$rolled" = "111" ] || { echo "FAIL: expected 111 rows after rollback (110 + 1 new), got '$rolled'"; exit 1; }
abandoned=$(psql_on "$BLUE_PRIMARY" appdb "SELECT count(*) FROM t WHERE v = 'post-cutover'")
[ "$abandoned" = "0" ] || { echo "FAIL: the abandoned post-cutover write is still present"; exit 1; }
for _ in $(seq 1 60); do
  $KUBECTL -n pglord get sts "$CLUSTER-$GEN_A" >/dev/null 2>&1 || break
  sleep 2
done
$KUBECTL -n pglord get sts "$CLUSTER-$GEN_A" >/dev/null 2>&1 && { echo "FAIL: the new instance set survived the rollback"; exit 1; }
echo "OK rolled back, writes resumed, new instance set deleted"

# ---------------------------------------------------------------- scenario B
echo "==> scenario B: start, cut over, finalize"
code=$(start_upgrade)
[ "$code" = "202" ] || { echo "FAIL: second upgrade start returned $code"; exit 1; }
GEN_B=$(upgrade_field next_generation)
[ -n "$GEN_B" ] || { echo "FAIL: no next_generation on the second upgrade"; exit 1; }
wait_ready_to_cutover || exit 1
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade/cutover")
[ "$code" = "202" ] || { echo "FAIL: second cutover returned $code"; exit 1; }
wait_phase soaking || exit 1
GREEN_PRIMARY="$CLUSTER-$GEN_B-0"
[ "$(primary_pod)" = "$GREEN_PRIMARY" ] || { echo "FAIL: primary is $(primary_pod), expected $GREEN_PRIMARY"; exit 1; }

echo "==> sequences continue past the old maximum"
maxid=$(psql_on "$GREEN_PRIMARY" appdb "SELECT max(id) FROM t")
newid=$(psql_on "$GREEN_PRIMARY" appdb "INSERT INTO t(v) VALUES ('after-upgrade') RETURNING id")
[ -n "$newid" ] || { echo "FAIL: insert after cutover produced no id"; exit 1; }
[ "$newid" -gt "$maxid" ] || { echo "FAIL: sequence handed out $newid, not past the existing max $maxid"; exit 1; }
echo "OK sequence continues ($maxid -> $newid)"

echo "==> pgcrypto works on the new major"
digest=$(psql_on "$GREEN_PRIMARY" appdb "SELECT encode(digest('x','sha256'),'hex')")
[ -n "$digest" ] || { echo "FAIL: pgcrypto is not usable after the upgrade"; exit 1; }

echo "==> finalizing"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade/finalize")
[ "$code" = "202" ] || { echo "FAIL: finalize returned $code"; exit 1; }
wait_no_upgrade || exit 1
for _ in $(seq 1 60); do
  $KUBECTL -n pglord get sts "$CLUSTER" >/dev/null 2>&1 || break
  sleep 2
done
$KUBECTL -n pglord get sts "$CLUSTER" >/dev/null 2>&1 && { echo "FAIL: the old instance set survived finalize"; exit 1; }
[ "$(field image)" = "pglord-agent:dev17" ] || { echo "FAIL: cluster image is $(field image), expected pglord-agent:dev17"; exit 1; }
ready=""
for _ in $(seq 1 120); do
  ready=$(field ready_replicas)
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: upgraded cluster never reached 2/2 (got '$ready')"; exit 1; }
final=$(psql_on "$(primary_pod)" appdb "SELECT count(*) FROM t")
[ "$final" = "112" ] || { echo "FAIL: expected 112 rows after finalize, got '$final'"; exit 1; }
echo "OK finalized: old set deleted, $CLUSTER-$GEN_B is the cluster at 2/2"

echo "PASS: major version upgrade e2e"
