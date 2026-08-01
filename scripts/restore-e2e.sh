#!/usr/bin/env bash
# Restore e2e: back a cluster up, then restore it into new clusters — once to
# the end of the WAL, once to a point in time — and prove the two land on
# different data. The point-in-time assertion is the one that matters: anything
# less would also pass with a plain file copy.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18104
BUCKET=pglord-backups
# Each run gets its own repository path: a previous run's stanza belongs to a
# different database and pgBackRest rejects the mismatched system identifier.
PREFIX="rs-$$"

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  for c in src dst pitr snap; do
    curl -s -X DELETE "$API/api/v1/clusters/$c" >/dev/null 2>&1 || true
  done
  kill_pf
}
trap cleanup EXIT

field() { curl -s "$API/api/v1/clusters/$1$2" | python3 -c "import sys,json; print(json.load(sys.stdin)$3)" 2>/dev/null || echo ""; }
mc() { $KUBECTL -n pglord exec deploy/minio -- mc "$@" 2>/dev/null; }
# Local socket connections use peer auth, so these need no password.
psql_on() { $KUBECTL -n pglord exec "$1" -- psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }

wait_ready() { # cluster timeout_iterations
  for _ in $(seq 1 "$2"); do
    [ "$(field "$1" '' "['ready_replicas']")" = "1" ] && return 0
    sleep 2
  done
  echo "FAIL: cluster '$1' never became ready"
  $KUBECTL -n pglord get pods
  $KUBECTL -n pglord logs "$1-0" --tail 40 || true
  return 1
}

wait_restored() { # cluster timeout_iterations
  # Ready is not enough: a restoring instance accepts read-only connections
  # while it is still replaying WAL, so asserting on row counts at that point
  # would race the recovery and pass or fail at random.
  wait_ready "$1" "$2" || return 1
  for _ in $(seq 1 "$2"); do
    [ "$(psql_on "$1-0" "SELECT pg_is_in_recovery()")" = "f" ] && return 0
    sleep 2
  done
  echo "FAIL: cluster '$1' never finished recovery"
  $KUBECTL -n pglord logs "$1-0" --tail 40 || true
  return 1
}

echo "==> building + loading images"
AGENT_IMAGE="pglord-agent:e2e-$$"
make image >/dev/null
docker build -q -f deploy/Dockerfile.agent -t "$AGENT_IMAGE" . >/dev/null
$KUBECTL apply -f deploy/k8s/namespace.yaml >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=0 >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=delete pod -l app=pglord-server --timeout=60s >/dev/null 2>&1 || true
minikube -p pglord image rm docker.io/library/pglord-server:dev >/dev/null 2>&1 || true
minikube -p pglord image load pglord-server:dev
minikube -p pglord image load "$AGENT_IMAGE"
$KUBECTL apply -f deploy/k8s/ >/dev/null
$KUBECTL -n pglord scale deploy/pglord-server --replicas=1 >/dev/null
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s >/dev/null

echo "==> starting MinIO (dev object storage)"
$KUBECTL apply -f deploy/k8s/dev/minio.yaml >/dev/null
$KUBECTL -n pglord rollout status deploy/minio --timeout=180s >/dev/null
mc alias set --insecure local https://localhost:9000 pglordtest pglordtest123 >/dev/null
mc mb --insecure --ignore-existing "local/$BUCKET" >/dev/null
echo "OK MinIO ready with bucket $BUCKET"

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18104:80 >/dev/null 2>&1 &
sleep 2

REPO="\"s3_endpoint\":\"https://minio.pglord.svc.cluster.local:9000\",\"verify_tls\":false,\"s3_bucket\":\"$BUCKET\",\"path_prefix\":\"$PREFIX\",\"s3_credentials_secret\":\"backup-credentials\",\"retention_full\":2"

echo "==> creating source cluster with backups"
code=$(curl -s -o /tmp/rs.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"src\",\"replicas\":1,\"image\":\"$AGENT_IMAGE\",\"backup\":{\"enabled\":true,$REPO}}")
[ "$code" = "201" ] || { echo "FAIL: create returned $code: $(cat /tmp/rs.json)"; exit 1; }
wait_ready src 180 || exit 1

echo "==> restarting so WAL archiving takes effect"
curl -s -o /dev/null -X POST "$API/api/v1/clusters/src/restart"
$KUBECTL -n pglord wait --for=delete pod/src-0 --timeout=120s >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=condition=Ready pod/src-0 --timeout=180s >/dev/null 2>&1 || true
for _ in $(seq 1 60); do
  [ "$(psql_on src-0 "SHOW archive_mode")" = "on" ] && break
  sleep 2
done
[ "$(psql_on src-0 "SHOW archive_mode")" = "on" ] || { echo "FAIL: archiving never came on"; exit 1; }
echo "OK archiving enabled"

echo "==> writing the row that must survive, then backing up"
psql_on src-0 "CREATE TABLE marks (id int primary key)" >/dev/null
psql_on src-0 "INSERT INTO marks VALUES (1)" >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/src/backups")
[ "$code" = "202" ] || { echo "FAIL: backup returned $code"; exit 1; }
for _ in $(seq 1 90); do
  [ "$(field src '/backups' "['backups'].__len__()")" = "1" ] && break
  sleep 4
done
[ "$(field src '/backups' "['backups'].__len__()")" = "1" ] || {
  echo "FAIL: backup never appeared"
  curl -s "$API/api/v1/clusters/src/backups" | python3 -m json.tool | head -30
  exit 1
}
LABEL=$(field src '/backups' "['backups'][0]['label']")
echo "OK backup taken: $LABEL"

echo "==> marking a point in time, then writing a row that must NOT survive it"
# The database's own clock, not the host's: recovery_target_time is compared
# against commit timestamps recorded by this server.
sleep 2
CUT=$(psql_on src-0 "SELECT to_char(now() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
[ -n "$CUT" ] || { echo "FAIL: could not read the database clock"; exit 1; }
sleep 2
psql_on src-0 "INSERT INTO marks VALUES (2)" >/dev/null
psql_on src-0 "SELECT pg_switch_wal()" >/dev/null
# The WAL holding row 2 has to reach the repository before we can recover past
# the backup; give archiving a moment to push it.
for _ in $(seq 1 30); do
  listing=$(mc ls --insecure --recursive "local/$BUCKET/$PREFIX")
  case "$listing" in *archive*) archived=yes; break ;; esac
  psql_on src-0 "SELECT pg_switch_wal()" >/dev/null
  sleep 2
done
[ "${archived:-}" = "yes" ] || { echo "FAIL: no WAL reached the repository"; exit 1; }
[ "$(psql_on src-0 "SELECT count(*) FROM marks")" = "2" ] || { echo "FAIL: source should hold 2 rows"; exit 1; }
echo "OK source has 2 rows, cut-off recorded at $CUT"

echo "==> refusing a restore that would overwrite the source"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/src/restore" \
  -H 'content-type: application/json' -d '{"target_cluster":"src"}')
[ "$code" = "409" ] || { echo "FAIL: in-place restore returned $code, expected 409"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/src/restore" \
  -H 'content-type: application/json' -d '{"target_cluster":"Bad_Name"}')
[ "$code" = "422" ] || { echo "FAIL: invalid target name returned $code, expected 422"; exit 1; }
echo "OK in-place and invalid restores refused"

echo "==> restoring to the end of the WAL"
code=$(curl -s -o /tmp/rs2.json -w '%{http_code}' -X POST "$API/api/v1/clusters/src/restore" \
  -H 'content-type: application/json' -d '{"target_cluster":"dst"}')
[ "$code" = "201" ] || { echo "FAIL: restore returned $code: $(cat /tmp/rs2.json)"; exit 1; }
wait_restored dst 240 || exit 1
got=$(psql_on dst-0 "SELECT count(*) FROM marks")
[ "$got" = "2" ] || { echo "FAIL: restored cluster has $got rows, expected 2"; exit 1; }
echo "OK restored to end of WAL: both rows present"

echo "==> the restored cluster must not archive into the source's repository"
# It inherits the source's PGDATA, archive_command included. Left alone it
# would push a diverging timeline into backups the source still depends on.
mode=$(psql_on dst-0 "SHOW archive_mode")
[ "$mode" = "off" ] || { echo "FAIL: restored cluster has archive_mode=$mode, expected off"; exit 1; }
echo "OK inherited archiving neutralised"

echo "==> the restored cluster answers to its own password"
# The restored data carries the SOURCE's roles; without a reset this cluster's
# generated Secret would not open it.
DST_PW=$($KUBECTL -n pglord get secret dst-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
auth=$($KUBECTL -n pglord exec dst-0 -- env PGPASSWORD="$DST_PW" psql -h dst-rw -U postgres -tAc "SELECT 1" 2>&1 || true)
case "$auth" in
  *1*) ;;
  *) echo "FAIL: could not authenticate with the restored cluster's own password: $auth"; exit 1 ;;
esac
echo "OK restored cluster uses its own credentials"

echo "==> restoring to the recorded point in time"
code=$(curl -s -o /tmp/rs3.json -w '%{http_code}' -X POST "$API/api/v1/clusters/src/restore" \
  -H 'content-type: application/json' -d "{\"target_cluster\":\"pitr\",\"target_time\":\"$CUT\"}")
[ "$code" = "201" ] || { echo "FAIL: PITR restore returned $code: $(cat /tmp/rs3.json)"; exit 1; }
wait_restored pitr 240 || exit 1
got=$(psql_on pitr-0 "SELECT count(*) FROM marks")
# THE assertion: recovery stopped at the cut-off, so the row written after it
# is absent. A restore that ignored the target would report 2 here.
[ "$got" = "1" ] || { echo "FAIL: point-in-time restore has $got rows, expected 1"; exit 1; }
[ "$(psql_on pitr-0 "SELECT id FROM marks")" = "1" ] || { echo "FAIL: wrong row survived"; exit 1; }
echo "OK point-in-time restore stopped at the cut-off: only row 1"

echo "==> restoring a named backup stops at that backup"
# pgBackRest's default recovery type replays every WAL record after the chosen
# backup, so without --type=immediate this returns 2 rows and "restore this
# backup" quietly means "restore everything".
code=$(curl -s -o /tmp/rs4.json -w '%{http_code}' -X POST "$API/api/v1/clusters/src/restore" \
  -H 'content-type: application/json' -d "{\"target_cluster\":\"snap\",\"backup_label\":\"$LABEL\"}")
[ "$code" = "201" ] || { echo "FAIL: named-backup restore returned $code: $(cat /tmp/rs4.json)"; exit 1; }
wait_restored snap 240 || exit 1
got=$(psql_on snap-0 "SELECT count(*) FROM marks")
[ "$got" = "1" ] || { echo "FAIL: named-backup restore has $got rows, expected 1"; exit 1; }
echo "OK named backup restored without rolling forward"

echo "==> the source is untouched"
[ "$(psql_on src-0 "SELECT count(*) FROM marks")" = "2" ] || { echo "FAIL: source changed"; exit 1; }
echo "OK source still has 2 rows"

echo "==> cleanup"
for c in src dst pitr snap; do curl -s -X DELETE "$API/api/v1/clusters/$c" >/dev/null; done
echo "restore e2e passed"
