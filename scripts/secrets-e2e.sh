#!/usr/bin/env bash
# Secrets e2e: a cluster that uses the user's own credentials Secret, custom
# environment from a Secret, and init scripts from a ConfigMap.
#
# The assertion that matters is the last link of the chain: connecting as a role
# that only exists because an init script created it, with a password that
# reached that script from a Secret by way of a custom environment variable.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18105

SU_PW=superpw$$
REPL_PW=replpw$$
APP_PW=apppw$$

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  for c in sec miss rot; do
    curl -s -X DELETE "$API/api/v1/clusters/$c" >/dev/null 2>&1 || true
  done
  $KUBECTL -n pglord delete secret sec-own-creds app-creds sec-init-secret --ignore-not-found >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete configmap sec-init --ignore-not-found >/dev/null 2>&1 || true
  kill_pf
}
trap cleanup EXIT

field() { curl -s "$API/api/v1/clusters/$1$2" | python3 -c "import sys,json; print(json.load(sys.stdin)$3)" 2>/dev/null || echo ""; }
# Local socket connections use peer auth, so these need no password.
psql_on() { $KUBECTL -n pglord exec "$1" -- psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }

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

kill_pf
$KUBECTL -n pglord port-forward svc/pglord-server 18105:80 >/dev/null 2>&1 &
sleep 2

echo "==> creating the objects the cluster will depend on"
$KUBECTL -n pglord delete secret sec-own-creds app-creds sec-init-secret --ignore-not-found >/dev/null 2>&1 || true
$KUBECTL -n pglord delete configmap sec-init --ignore-not-found >/dev/null 2>&1 || true
$KUBECTL -n pglord create secret generic sec-own-creds \
  --from-literal=superuser-password="$SU_PW" \
  --from-literal=replication-password="$REPL_PW" >/dev/null
$KUBECTL -n pglord create secret generic app-creds \
  --from-literal=password="$APP_PW" >/dev/null
# The init script never sees the password itself: it interpolates a psql
# variable that the agent filled in from the container's environment.
cat >/tmp/01-app.sql <<'SQL'
CREATE ROLE app LOGIN PASSWORD :'APP_PASSWORD';
CREATE TABLE init_marker (note text);
INSERT INTO init_marker VALUES ('created by init script');
SQL
$KUBECTL -n pglord create configmap sec-init --from-file=/tmp/01-app.sql >/dev/null
# Scripts from a Secret: this is the source people use for init SQL that
# contains credentials, and the one whose file permissions inside the pod are
# easy to get wrong — Kubernetes owns projected secret files as root while the
# container runs as postgres.
cat >/tmp/02-reporter.sql <<'SQL'
CREATE ROLE reporter LOGIN PASSWORD :'APP_PASSWORD';
INSERT INTO init_marker VALUES ('created by secret script');
SQL
$KUBECTL -n pglord create secret generic sec-init-secret --from-file=/tmp/02-reporter.sql >/dev/null
echo "OK secrets, init ConfigMap and init Secret created"

echo "==> rejecting an environment variable pglord owns"
code=$(curl -s -o /tmp/sec-bad.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"bad\",\"image\":\"$AGENT_IMAGE\",
    \"env\":[{\"name\":\"PGLORD_AGENT_ROLE\",\"value\":\"primary\"}]}")
[ "$code" = "422" ] || { echo "FAIL: reserved env returned $code, expected 422: $(cat /tmp/sec-bad.json)"; exit 1; }
# PGDATA would point postgres at a different directory entirely.
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"bad\",\"image\":\"$AGENT_IMAGE\",
    \"env\":[{\"name\":\"PGDATA\",\"value\":\"/tmp\"}]}")
[ "$code" = "422" ] || { echo "FAIL: PGDATA env returned $code, expected 422"; exit 1; }
# A variable with neither a value nor a secret reference is meaningless.
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"bad\",\"image\":\"$AGENT_IMAGE\",\"env\":[{\"name\":\"APP\"}]}")
[ "$code" = "422" ] || { echo "FAIL: sourceless env returned $code, expected 422"; exit 1; }
echo "OK reserved and malformed environment refused"

echo "==> reporting a dependency that does not exist"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"miss\",\"image\":\"$AGENT_IMAGE\",
    \"env\":[{\"name\":\"NOPE\",\"secret\":{\"name\":\"absent-secret\",\"key\":\"k\"}}]}")
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
satisfied=$(field miss '/secrets' "['satisfied']")
[ "$satisfied" = "False" ] || { echo "FAIL: missing secret reported satisfied=$satisfied"; exit 1; }
exists=$(curl -s "$API/api/v1/clusters/miss/secrets" | python3 -c "
import sys,json
for r in json.load(sys.stdin)['resources']:
    if r['name'] == 'absent-secret':
        print(r['exists']); break
else:
    print('not-listed')")
[ "$exists" = "False" ] || { echo "FAIL: absent secret reported exists=$exists"; exit 1; }
curl -s -X DELETE "$API/api/v1/clusters/miss" >/dev/null
echo "OK a missing dependency is reported before it becomes a stuck pod"

echo "==> creating a cluster with own credentials, custom env and init scripts"
code=$(curl -s -o /tmp/sec.json -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' -d "{
    \"name\":\"sec\",\"replicas\":1,\"image\":\"$AGENT_IMAGE\",
    \"credentials_secret\":\"sec-own-creds\",
    \"env\":[{\"name\":\"APP_PASSWORD\",\"secret\":{\"name\":\"app-creds\",\"key\":\"password\"}}],
    \"init\":{\"scripts_config_map\":\"sec-init\",\"scripts_secret\":\"sec-init-secret\"}}")
[ "$code" = "201" ] || { echo "FAIL: create returned $code: $(cat /tmp/sec.json)"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field sec '' "['ready_replicas']")" = "1" ] && break
  sleep 2
done
[ "$(field sec '' "['ready_replicas']")" = "1" ] || {
  echo "FAIL: cluster never became ready"
  $KUBECTL -n pglord get pods
  $KUBECTL -n pglord logs sec-0 --tail 40 || true
  exit 1
}
echo "OK cluster ready"

echo "==> pglord did not generate a credentials secret of its own"
# Generating one alongside the supplied Secret would give the cluster two
# sources of truth for its password.
if $KUBECTL -n pglord get secret sec-credentials >/dev/null 2>&1; then
  echo "FAIL: sec-credentials was generated despite a supplied credentials secret"; exit 1
fi
echo "OK no generated credentials secret"

echo "==> the supplied superuser password is the one that works"
auth=$($KUBECTL -n pglord exec sec-0 -- env PGPASSWORD="$SU_PW" psql -h sec-rw -U postgres -tAc "SELECT 'ok'" 2>&1 || true)
case "$auth" in
  *ok*) ;;
  *) echo "FAIL: supplied superuser password rejected: $auth"; exit 1 ;;
esac
echo "OK supplied credentials accepted"

echo "==> the init script ran"
# Scoped to the one row: the Secret-sourced script writes to this table too.
marker=$(psql_on sec-0 "SELECT note FROM init_marker WHERE note = 'created by init script'")
[ "$marker" = "created by init script" ] || { echo "FAIL: init marker is '$marker'"; $KUBECTL -n pglord logs sec-0 --tail 40; exit 1; }
echo "OK init script ran"

echo "==> the init script used the secret it was given"
# The whole chain in one assertion: Secret -> env var -> psql variable ->
# CREATE ROLE. Connecting as that role with that password proves every link.
auth=$($KUBECTL -n pglord exec sec-0 -- env PGPASSWORD="$APP_PW" psql -h sec-rw -U app -d postgres -tAc "SELECT 'app-ok'" 2>&1 || true)
case "$auth" in
  *app-ok*) ;;
  *) echo "FAIL: could not connect as the role the init script created: $auth"; exit 1 ;;
esac
echo "OK secret reached the init script and became a working role"

echo "==> scripts from a Secret run too, and run after the ConfigMap's"
# Readable by postgres despite Kubernetes owning projected secret files as
# root: an owner-only mode here would leave these scripts unrunnable.
auth=$($KUBECTL -n pglord exec sec-0 -- env PGPASSWORD="$APP_PW" psql -h sec-rw -U reporter -d postgres -tAc "SELECT 'reporter-ok'" 2>&1 || true)
case "$auth" in
  *reporter-ok*) ;;
  *) echo "FAIL: the Secret-sourced init script did not run: $auth"; $KUBECTL -n pglord logs sec-0 --tail 40; exit 1 ;;
esac
# Ordering: the Secret's script inserts into a table the ConfigMap's script
# creates. Had it run first, it would have failed on a missing table.
notes=$(psql_on sec-0 "SELECT string_agg(note, '|' ORDER BY note) FROM init_marker")
[ "$notes" = "created by init script|created by secret script" ] || {
  echo "FAIL: init_marker holds '$notes'"; exit 1
}
echo "OK Secret scripts ran, after the ConfigMap's"

echo "==> dependencies are all reported present"
satisfied=$(field sec '/secrets' "['satisfied']")
[ "$satisfied" = "True" ] || {
  echo "FAIL: dependencies reported unsatisfied"
  curl -s "$API/api/v1/clusters/sec/secrets" | python3 -m json.tool
  exit 1
}
managed=$(curl -s "$API/api/v1/clusters/sec/secrets" | python3 -c "
import sys,json
for r in json.load(sys.stdin)['resources']:
    if r['name'] == 'sec-own-creds':
        print(r['managed']); break")
[ "$managed" = "False" ] || { echo "FAIL: supplied secret reported as pglord-managed"; exit 1; }
echo "OK dependencies satisfied, supplied secret not claimed as managed"

echo "==> init scripts do not run again on restart"
# They create roles and tables; a second run would fail on the duplicate and
# leave the instance looking broken.
# Two rows from the two init scripts, plus this one.
psql_on sec-0 "INSERT INTO init_marker VALUES ('written after init')" >/dev/null
curl -s -o /dev/null -X POST "$API/api/v1/clusters/sec/restart"
$KUBECTL -n pglord wait --for=delete pod/sec-0 --timeout=120s >/dev/null 2>&1 || true
$KUBECTL -n pglord wait --for=condition=Ready pod/sec-0 --timeout=180s >/dev/null 2>&1 || true
for _ in $(seq 1 60); do
  [ "$(psql_on sec-0 "SELECT count(*) FROM init_marker")" = "3" ] && break
  sleep 2
done
rows=$(psql_on sec-0 "SELECT count(*) FROM init_marker")
# A second run would also have failed outright on the duplicate CREATE ROLE.
[ "$rows" = "3" ] || { echo "FAIL: init_marker has $rows rows after restart, expected 3"; exit 1; }
echo "OK init scripts ran once, not again"

echo "==> rotation refuses a cluster with a user-supplied secret"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/sec/rotate-credentials")
[ "$code" = "409" ] || { echo "FAIL: rotate on custom-secret cluster -> $code, expected 409"; exit 1; }
echo "OK custom-secret cluster refused"

echo "==> rotating a managed cluster"
curl -s -X POST "$API/api/v1/clusters" -H 'content-type: application/json' \
  -d "{\"name\":\"rot\",\"replicas\":2,\"image\":\"$AGENT_IMAGE\"}" >/dev/null
for _ in $(seq 1 90); do
  [ "$(field rot '' "['ready_replicas']")" = "2" ] && break
  sleep 2
done
[ "$(field rot '' "['ready_replicas']")" = "2" ] || { echo "FAIL: rot never became ready"; exit 1; }
old_pw=$($KUBECTL -n pglord get secret rot-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
old_uid0=$($KUBECTL -n pglord get pod rot-0 -o jsonpath='{.metadata.uid}')
old_uid1=$($KUBECTL -n pglord get pod rot-1 -o jsonpath='{.metadata.uid}')
curl -s -o /dev/null -X POST "$API/api/v1/clusters/rot/rotate-credentials"
# Rotation rolls both pods; wait for the secret to change, then for readiness.
for _ in $(seq 1 90); do
  new_pw=$($KUBECTL -n pglord get secret rot-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
  [ "$new_pw" != "$old_pw" ] && break
  sleep 2
done
[ "$new_pw" != "$old_pw" ] || { echo "FAIL: secret password never changed"; exit 1; }
rotated_at=$($KUBECTL -n pglord get secret rot-credentials -o jsonpath='{.metadata.annotations.pglord\.dev/rotated-at}')
[ -n "$rotated_at" ] || { echo "FAIL: rotated-at annotation missing"; exit 1; }
# "ready_replicas=2 at some instant" is NOT "roll finished": both pods are
# briefly ready between the standby's return and the primary's deletion. The
# roll deletes each pod exactly once, so completion = both pods are NEW
# (UIDs changed) and the cluster is ready.
rolled() {
  uid0=$($KUBECTL -n pglord get pod rot-0 -o jsonpath='{.metadata.uid}' 2>/dev/null || echo "")
  uid1=$($KUBECTL -n pglord get pod rot-1 -o jsonpath='{.metadata.uid}' 2>/dev/null || echo "")
  [ -n "$uid0" ] && [ -n "$uid1" ] \
    && [ "$uid0" != "$old_uid0" ] && [ "$uid1" != "$old_uid1" ] \
    && [ "$(field rot '' "['ready_replicas']")" = "2" ]
}
for _ in $(seq 1 120); do
  rolled && break
  sleep 2
done
rolled || { echo "FAIL: rot not rolled+ready after rotation"; exit 1; }
# TCP auth uses scram: the new password must work, the old must not.
ok=$($KUBECTL -n pglord exec rot-0 -- env PGPASSWORD="$new_pw" psql -h rot-rw -U postgres -tAc "SELECT 1" 2>/dev/null || echo "")
[ "$ok" = "1" ] || { echo "FAIL: new password rejected"; exit 1; }
bad=$($KUBECTL -n pglord exec rot-0 -- env PGPASSWORD="$old_pw" psql -h rot-rw -U postgres -tAc "SELECT 1" 2>/dev/null || echo "")
[ "$bad" != "1" ] || { echo "FAIL: old password still accepted"; exit 1; }
# The standby must still be streaming after the roll (conninfo refreshed).
# The primary may be either pod after the roll; check both.
streaming=$(psql_on rot-0 "SELECT count(*) FROM pg_stat_replication")
[ "$streaming" = "1" ] || streaming=$(psql_on rot-1 "SELECT count(*) FROM pg_stat_replication")
[ "$streaming" = "1" ] || { echo "FAIL: standby not streaming after rotation (got '$streaming')"; exit 1; }
echo "OK rotation: new password works, old rejected, standby streaming"
curl -s -X DELETE "$API/api/v1/clusters/rot" >/dev/null

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/sec" >/dev/null
echo "secrets e2e passed"
