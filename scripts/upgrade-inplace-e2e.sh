#!/usr/bin/env bash
# In-place major upgrade e2e: PostgreSQL 16 -> 17 with pg_upgrade --link.
# Needs the dev MinIO (deploy/k8s/dev/minio.yaml) because in-place refuses to
# start without a completed backup.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18106
CLUSTER=ipdemo
BUCKET=pglord-inplace
# A stanza belongs to one database (system id). The bucket outlives a run, so
# each run needs its own repository path or pgBackRest correctly refuses to
# reuse the previous cluster's stanza.
PREFIX="inplace-$$"

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
# KEEP=1 leaves the cluster up so a failure can be inspected. Two runs must
# never overlap: this trap deletes the cluster by name, so a second run's
# cluster would be destroyed by the first one exiting.
cleanup() {
  if [ -z "${KEEP:-}" ]; then
    curl -s -X DELETE "$API/api/v1/clusters/$CLUSTER" >/dev/null 2>&1 || true
  else
    echo "KEEP=1: leaving $CLUSTER up for inspection"
  fi
  kill_pf
}
trap cleanup EXIT

detail() { curl -s "$API/api/v1/clusters/$CLUSTER"; }
field() {
  detail | python3 -c "import sys,json; print(json.load(sys.stdin).get('$1',''))" 2>/dev/null || echo ""
}
upgrade_field() {
  detail | python3 -c "import sys,json; u=(json.load(sys.stdin).get('upgrade') or {}); print(u.get('$1',''))" 2>/dev/null || echo ""
}
mc() { $KUBECTL -n pglord exec deploy/minio -- mc "$@" 2>/dev/null; }
psql_on() { # pod db sql
  $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" \
    psql -U postgres -d "$2" -tAc "$3" 2>/dev/null || echo ""
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
for img in pglord-server:dev pglord-agent:dev16 pglord-agent:dev17; do
  want=$(docker image inspect "$img" -f '{{.Id}}')
  have=$(minikube -p pglord ssh -- docker image inspect -f '{{.Id}}' "docker.io/library/$img" 2>/dev/null | tr -d '\r')
  [ "$want" = "$have" ] || { echo "FAIL: minikube kept a stale $img (have $have, want $want)"; exit 1; }
done
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

echo "==> starting MinIO (dev object storage)"
$KUBECTL apply -f deploy/k8s/dev/minio.yaml >/dev/null
$KUBECTL -n pglord rollout status deploy/minio --timeout=180s >/dev/null
mc alias set --insecure local https://localhost:9000 pglordtest pglordtest123 >/dev/null
mc mb --insecure --ignore-existing "local/$BUCKET" >/dev/null

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18106:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating '$CLUSTER' on PostgreSQL 16 (replicas=2, backups on)"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\": \"$CLUSTER\", \"image\": \"pglord-agent:dev16\", \"replicas\": 2,
    \"roles\": [{\"name\": \"app_rw\"}],
    \"databases\": [{\"name\": \"appdb\", \"owner\": \"app_rw\"}],
    \"extensions\": [{\"name\": \"pgcrypto\", \"database\": \"appdb\"}],
    \"backup\": {
      \"enabled\": true,
      \"s3_endpoint\": \"https://minio.pglord.svc.cluster.local:9000\",
      \"verify_tls\": false,
      \"s3_bucket\": \"$BUCKET\",
      \"path_prefix\": \"$PREFIX\",
      \"s3_credentials_secret\": \"backup-credentials\"
    }
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
PRIMARY=$(primary_pod)
echo "OK cluster ready (primary $PRIMARY)"

ver=$(psql_on "$PRIMARY" postgres "SHOW server_version_num")
case "$ver" in 16*) ;; *) echo "FAIL: expected PostgreSQL 16, got $ver"; exit 1;; esac

echo "==> waiting for provisioning, then seeding"
have=""
for _ in $(seq 1 60); do
  have=$(psql_on "$PRIMARY" appdb "SELECT count(*) FROM pg_extension WHERE extname='pgcrypto'")
  [ "$have" = "1" ] && break
  sleep 2
done
[ "$have" = "1" ] || { echo "FAIL: appdb/pgcrypto never provisioned"; exit 1; }
psql_on "$PRIMARY" appdb \
  "CREATE TABLE t(id bigserial primary key, v text); \
   INSERT INTO t(v) SELECT 'row-' || g FROM generate_series(1,100) g;" >/dev/null
seeded=$(psql_on "$PRIMARY" appdb "SELECT count(*) FROM t")
[ "$seeded" = "100" ] || { echo "FAIL: expected 100 rows, got '$seeded'"; exit 1; }
echo "OK seeded"

start_in_place() {
  curl -s -X POST "$API/api/v1/clusters/$CLUSTER/upgrade" -H 'content-type: application/json' \
    -d '{"target_image":"pglord-agent:dev17","mode":"in_place"}'
}

echo "==> in-place is refused before any backup exists"
body=$(start_in_place)
case "$body" in
  *backup_required*) ;;
  *) echo "FAIL: expected backup_required, got: $body"; exit 1;;
esac
echo "OK refused with backup_required"

echo "==> restarting so WAL archiving takes effect"
# archive_mode is a restart-only setting and the archive config is written
# after postgres first boots, so a cluster created with backups on still needs
# one restart before pgBackRest will accept a backup.
curl -s -o /dev/null -X POST "$API/api/v1/clusters/$CLUSTER/restart"
$KUBECTL -n pglord wait --for=delete "pod/$CLUSTER-1" --timeout=120s >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=condition=Ready "pod/$CLUSTER-1" --timeout=180s >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete "pod/$CLUSTER-0" --timeout=120s >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=condition=Ready "pod/$CLUSTER-0" --timeout=180s >/dev/null 2>&1 || true
archive=""
for _ in $(seq 1 90); do
  archive=$(psql_on "$(primary_pod)" postgres "SHOW archive_mode")
  [ "$archive" = "on" ] && break
  sleep 2
done
[ "$archive" = "on" ] || { echo "FAIL: archive_mode is '$archive', expected on"; exit 1; }
echo "OK archiving enabled"

echo "==> taking a backup"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/backups" \
  -H 'content-type: application/json' -d '{"kind":"full"}')
[ "$code" = "202" ] || { echo "FAIL: backup returned $code"; exit 1; }
state=""
for _ in $(seq 1 120); do
  state=$(curl -s "$API/api/v1/clusters/$CLUSTER/backups" | python3 -c \
    "import sys,json; r=json.load(sys.stdin).get('runs') or []; print(r[0]['state'] if r else '')" 2>/dev/null || echo "")
  [ "$state" = "succeeded" ] && break
  if [ "$state" = "failed" ]; then
    echo "FAIL: the backup failed: $(curl -s "$API/api/v1/clusters/$CLUSTER/backups" | python3 -c \
      "import sys,json; r=json.load(sys.stdin).get('runs') or []; print(r[0].get('error','') if r else '')")"
    exit 1
  fi
  sleep 2
done
[ "$state" = "succeeded" ] || { echo "FAIL: backup never succeeded (last state '$state')"; exit 1; }
echo "OK backup succeeded"

echo "==> starting the in-place upgrade"
body=$(start_in_place)
case "$body" in
  "") ;;
  *problems*) echo "FAIL: in-place start refused: $body"; exit 1;;
esac
mode=$(upgrade_field mode)
[ "$mode" = "in_place" ] || { echo "FAIL: upgrade mode is '$mode', expected in_place"; exit 1; }
echo "OK in-place upgrade started"

echo "==> waiting for the conversion to settle"
for _ in $(seq 1 180); do
  [ -z "$(upgrade_field phase)" ] && break
  sleep 2
done
[ -z "$(upgrade_field phase)" ] || { echo "FAIL: the upgrade never settled"; exit 1; }
ready=""
for _ in $(seq 1 120); do
  ready=$(field ready_replicas)
  [ "$ready" = "2" ] && break
  sleep 2
done
[ "$ready" = "2" ] || { echo "FAIL: cluster never returned to 2/2 (got '$ready')"; exit 1; }
echo "OK settled and back to 2/2"

echo "==> the same instances now run PostgreSQL 17 with the data intact"
PRIMARY=$(primary_pod)
ver=$(psql_on "$PRIMARY" postgres "SHOW server_version_num")
case "$ver" in 17*) ;; *) echo "FAIL: expected PostgreSQL 17, got '$ver'"; exit 1;; esac
rows=$(psql_on "$PRIMARY" appdb "SELECT count(*) FROM t")
[ "$rows" = "100" ] || { echo "FAIL: expected 100 rows after the upgrade, got '$rows'"; exit 1; }
maxid=$(psql_on "$PRIMARY" appdb "SELECT max(id) FROM t")
newid=$(psql_on "$PRIMARY" appdb "INSERT INTO t(v) VALUES ('after') RETURNING id" | head -1)
[ -n "$newid" ] && [ "$newid" -gt "$maxid" ] || { echo "FAIL: sequence handed out '$newid', not past '$maxid'"; exit 1; }
digest=$(psql_on "$PRIMARY" appdb "SELECT length(encode(digest('x','sha256'),'hex'))")
[ "$digest" = "64" ] || { echo "FAIL: pgcrypto is not usable after the upgrade"; exit 1; }
[ "$(field image)" = "pglord-agent:dev17" ] || { echo "FAIL: cluster image is $(field image)"; exit 1; }
echo "OK PG17 with 100 rows, a continuing sequence and pgcrypto"

echo "==> object names are unchanged — this is what distinguishes in place"
$KUBECTL -n pglord get sts "$CLUSTER" >/dev/null 2>&1 || { echo "FAIL: the original StatefulSet is gone"; exit 1; }
if $KUBECTL -n pglord get sts "$CLUSTER-g2" >/dev/null 2>&1; then
  echo "FAIL: in-place created a second instance set"; exit 1
fi
case "$PRIMARY" in
  *-g2-*) echo "FAIL: primary is $PRIMARY — a generation suffix means a blue-green upgrade ran"; exit 1;;
esac
echo "OK still $CLUSTER / $PRIMARY"

echo "PASS: in-place major version upgrade e2e"
