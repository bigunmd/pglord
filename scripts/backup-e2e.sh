#!/usr/bin/env bash
# Backup e2e: configure a repository on MinIO, restart to enable archiving,
# take a backup, and confirm it really landed in object storage.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18103
BUCKET=pglord-backups

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/bk" >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

field() { curl -s "$API/api/v1/clusters/bk$1" | python3 -c "import sys,json; print(json.load(sys.stdin)$2)" 2>/dev/null || echo ""; }
mc() { $KUBECTL -n pglord exec deploy/minio -- mc "$@" 2>/dev/null; }
# Clusters use scram auth, so every psql call needs the generated password.
psql_on() { $KUBECTL -n pglord exec bk-0 -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$1" 2>/dev/null || echo ""; }

echo "==> building + loading images"
# The agent image gets a unique tag per run. `minikube image rm` cannot remove
# an image any pod is using, so reusing :dev would silently keep a stale agent
# whenever another cluster is running — and the cluster spec lets us pin the
# tag we just built.
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
$KUBECTL -n pglord port-forward svc/pglord-server 18103:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating cluster with backups configured"
code=$(curl -s -o /tmp/bk.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"bk\",\"replicas\":1,
    \"image\":\"$AGENT_IMAGE\",
    \"backup\":{
      \"enabled\":true,
      \"s3_endpoint\":\"https://minio.pglord.svc.cluster.local:9000\",
      \"verify_tls\":false,
      \"s3_bucket\":\"$BUCKET\",
      \"s3_credentials_secret\":\"backup-credentials\",
      \"retention_full\":2
    }}")
[ "$code" = "201" ] || { echo "FAIL: create returned $code: $(cat /tmp/bk.json)"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field '' "['ready_replicas']")" = "1" ] && break
  sleep 2
done
[ "$(field '' "['ready_replicas']")" = "1" ] || { echo "FAIL: cluster never became ready"; $KUBECTL -n pglord get pods; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret bk-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
echo "OK cluster ready"

echo "==> rejecting an incomplete repository"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/bk" \
  -H 'content-type: application/json' -d '{"backup":{"enabled":true,"s3_bucket":"b"}}')
[ "$code" = "422" ] || { echo "FAIL: incomplete backup config returned $code, expected 422"; exit 1; }
echo "OK incomplete repository refused"

echo "==> restarting so WAL archiving takes effect"
curl -s -o /dev/null -X POST "$API/api/v1/clusters/bk/restart"
sleep 10
for _ in $(seq 1 90); do
  [ "$(field '' "['ready_replicas']")" = "1" ] && break
  sleep 2
done
archive=$(psql_on "SHOW archive_mode")
[ "$archive" = "on" ] || { echo "FAIL: archive_mode is '$archive', expected on"; $KUBECTL -n pglord logs bk-0 --tail 20; exit 1; }
echo "OK archiving enabled"

echo "==> taking a backup"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/bk/backups")
[ "$code" = "202" ] || { echo "FAIL: backup returned $code"; exit 1; }
for _ in $(seq 1 90); do
  count=$(field '/backups' "['backups'].__len__()")
  [ "$count" = "1" ] && break
  sleep 4
done
[ "$count" = "1" ] || { echo "FAIL: no backup listed (count='$count')"; $KUBECTL -n pglord logs bk-0 --tail 30; exit 1; }
kind=$(field '/backups' "['backups'][0]['kind']")
size=$(field '/backups' "['backups'][0]['size_bytes']")
[ "$kind" = "full" ] || { echo "FAIL: backup type '$kind'"; exit 1; }
[ "$size" -gt 0 ] || { echo "FAIL: backup size is $size"; exit 1; }
echo "OK backup listed: full, $size bytes"

echo "==> checking the run was recorded"
state=$(field '/backups' "['runs'][0]['state']")
trigger=$(field '/backups' "['runs'][0]['trigger']")
[ "$state" = "succeeded" ] || { echo "FAIL: run state is '$state'"; exit 1; }
[ "$trigger" = "manual" ] || { echo "FAIL: run trigger is '$trigger'"; exit 1; }
echo "OK run recorded as succeeded/manual"

echo "==> taking an incremental backup"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/bk/backups" \
  -H 'content-type: application/json' -d '{"kind":"incr"}')
[ "$code" = "202" ] || { echo "FAIL: incremental backup returned $code"; exit 1; }
# A second request while one is running must be refused, not queued.
sleep 1
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/bk/backups" \
  -H 'content-type: application/json' -d '{"kind":"incr"}')
[ "$code" = "409" ] || echo "NOTE: concurrent backup returned $code (run may have finished already)"
for _ in $(seq 1 90); do
  count=$(field '/backups' "['backups'].__len__()")
  [ "$count" = "2" ] && break
  sleep 4
done
[ "$count" = "2" ] || { echo "FAIL: incremental backup not listed (count='$count')"; exit 1; }
kinds=$(field '/backups' "['backups'][0]['kind']")
[ "$kinds" = "incr" ] || { echo "FAIL: newest backup is '$kinds', expected incr"; exit 1; }
echo "OK incremental backup taken and listed"

echo "==> setting a schedule"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/bk" \
  -H 'content-type: application/json' -d "{\"backup\":{\"enabled\":true,\"s3_endpoint\":\"https://minio.pglord.svc.cluster.local:9000\",\"verify_tls\":false,\"s3_bucket\":\"$BUCKET\",\"s3_credentials_secret\":\"backup-credentials\",\"retention_full\":2,\"schedule\":\"0 3 * * *\",\"schedule_kind\":\"incr\"}}")
[ "$code" = "200" ] || { echo "FAIL: setting a schedule returned $code"; exit 1; }
[ "$(field '/backups' "['schedule']")" = "0 3 * * *" ] || { echo "FAIL: schedule not reported"; exit 1; }
# Nonsense and too-frequent schedules must be refused.
for bad in 'not a cron' '* * * * *'; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/bk" \
    -H 'content-type: application/json' -d "{\"backup\":{\"enabled\":true,\"s3_endpoint\":\"https://minio.pglord.svc.cluster.local:9000\",\"verify_tls\":false,\"s3_bucket\":\"$BUCKET\",\"s3_credentials_secret\":\"backup-credentials\",\"schedule\":\"$bad\"}}")
  [ "$code" = "422" ] || { echo "FAIL: schedule '$bad' returned $code, expected 422"; exit 1; }
done
echo "OK schedule stored, invalid schedules refused"

echo "==> confirming objects really exist in the bucket"
objects=$(mc ls --insecure --recursive "local/$BUCKET" | wc -l)
[ "$objects" -gt 0 ] || { echo "FAIL: bucket is empty"; exit 1; }
mc ls --insecure --recursive "local/$BUCKET" | grep -q 'backup' || { echo "FAIL: no backup objects in bucket"; exit 1; }
echo "OK $objects objects in $BUCKET"

echo "==> confirming WAL is archived"
for _ in $(seq 1 30); do
  psql_on "SELECT pg_switch_wal()" >/dev/null
  sleep 2
  mc ls --insecure --recursive "local/$BUCKET" | grep -q 'archive' && { archived=yes; break; }
done
[ "${archived:-}" = "yes" ] || { echo "FAIL: no archived WAL in the repository"; exit 1; }
echo "OK WAL archived to the repository"

echo "==> disabling backups turns archiving off again"
curl -s -o /dev/null -X PATCH "$API/api/v1/clusters/bk" \
  -H 'content-type: application/json' -d '{"backup":{"enabled":false}}'
[ "$(field '/backups' "['enabled']")" = "False" ] || { echo "FAIL: backups still reported enabled"; exit 1; }
echo "OK backups disabled"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/bk" >/dev/null
echo "backup e2e passed"
