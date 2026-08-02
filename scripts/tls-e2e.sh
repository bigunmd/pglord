#!/usr/bin/env bash
# TLS e2e: a cluster serving a referenced cert Secret accepts sslmode=require,
# a rotated Secret is served without any restart, cert hba rules are gated,
# and a hostnossl reject rule turns plaintext clients away.
set -euo pipefail

KUBECTL="minikube -p pglord kubectl --"
API=http://127.0.0.1:18098
CERTS=$(mktemp -d)

kill_pf() { pkill -f "kubectl.*port-forward.*pglord-server" 2>/dev/null || true; }
cleanup() {
  curl -s -X DELETE "$API/api/v1/clusters/tlse" >/dev/null 2>&1 || true
  $KUBECTL -n pglord delete secret tlse-cert --ignore-not-found >/dev/null 2>&1 || true
  rm -rf "$CERTS"
  kill_pf
}
trap cleanup EXIT

psql_on() { $KUBECTL -n pglord exec "$1" -- env PGPASSWORD="$SU_PW" psql -U postgres -tAc "$2" 2>/dev/null || echo ""; }
restarts_of() { $KUBECTL -n pglord get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?"; }
detail() { curl -s "$API/api/v1/clusters/tlse"; }
field() { detail | python3 -c "import sys,json; print(json.load(sys.stdin)$1)" 2>/dev/null || echo ""; }
# Network path with an explicit sslmode: exec on tlse-1, connect to tlse-0.
net_psql() { $KUBECTL -n pglord exec tlse-1 -- env PGPASSWORD="$SU_PW" PGSSLMODE="$1" psql -h tlse-0.tlse-pods -U postgres -tAc "$2" 2>/dev/null || echo "DENIED"; }
# Serial of the certificate tlse-0 actually serves right now.
served_serial() {
  $KUBECTL -n pglord exec tlse-1 -- sh -c \
    'openssl s_client -connect tlse-0.tlse-pods:5432 -starttls postgres </dev/null 2>/dev/null | openssl x509 -noout -serial' \
    2>/dev/null || echo "?"
}

gen_cert() { # $1: output prefix
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
    -keyout "$CERTS/$1.key" -out "$CERTS/$1.crt" \
    -subj "/CN=tlse-rw" >/dev/null 2>&1
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

echo "==> creating the cert secret and cluster 'tlse' (replicas=2, tls on)"
gen_cert server
$KUBECTL -n pglord create secret generic tlse-cert \
  --from-file=tls.crt="$CERTS/server.crt" \
  --from-file=tls.key="$CERTS/server.key" >/dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters" \
  -H 'content-type: application/json' \
  -d '{"name":"tlse","replicas":2,"tls":{"server_secret":"tlse-cert"}}')
[ "$code" = "201" ] || { echo "FAIL: create returned $code"; exit 1; }
for _ in $(seq 1 180); do
  [ "$(field "['ready_replicas']")" = "2" ] && break
  sleep 2
done
[ "$(field "['ready_replicas']")" = "2" ] || { echo "FAIL: never reached 2/2"; exit 1; }
SU_PW=$($KUBECTL -n pglord get secret tlse-credentials -o jsonpath='{.data.superuser-password}' | base64 -d)
echo "OK cluster ready"

echo "==> tls handshake works and the backend reports ssl"
# The agents converge the tls conf on their 5s poll after postgres starts, so
# the first connections may briefly be plaintext-only.
for _ in $(seq 1 30); do
  [ "$(net_psql require 'SELECT 1')" = "1" ] && break
  sleep 2
done
[ "$(net_psql require 'SELECT 1')" = "1" ] || { echo "FAIL: sslmode=require cannot connect"; exit 1; }
ssl_seen=$(net_psql require "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
[ "$ssl_seen" = "t" ] || { echo "FAIL: backend does not report ssl (got '$ssl_seen')"; exit 1; }
# The detail view carries the per-instance flag the UI badges on.
all_ssl() { detail | python3 -c "import sys,json; d=json.load(sys.stdin); print(bool(d['instances']) and all(i['ssl'] for i in d['instances']))" 2>/dev/null || echo ""; }
for _ in $(seq 1 15); do
  [ "$(all_ssl)" = "True" ] && break
  sleep 2
done
[ "$(all_ssl)" = "True" ] || { echo "FAIL: detail does not report ssl on every instance"; exit 1; }
echo "OK tls served, pg_stat_ssl and detail agree"

echo "==> cert hba rules are gated"
# cert on a plain host line can never match and is rejected outright.
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/tlse" \
  -H 'content-type: application/json' \
  -d '{"pg_hba":[{"conn":"host","database":"all","user":"all","address":"all","method":"cert"}]}')
[ "$code" = "422" ] || { echo "FAIL: cert-on-host returned $code, expected 422"; exit 1; }
echo "OK cert without hostssl rejected as 422"

echo "==> rotating the certificate reloads, not restarts"
before0=$(restarts_of tlse-0); before1=$(restarts_of tlse-1)
old_serial=$(served_serial)
[ "$old_serial" != "?" ] || { echo "FAIL: could not read the served certificate"; exit 1; }
gen_cert renewed
$KUBECTL -n pglord create secret generic tlse-cert \
  --from-file=tls.crt="$CERTS/renewed.crt" \
  --from-file=tls.key="$CERTS/renewed.key" \
  --dry-run=client -o yaml | $KUBECTL -n pglord apply -f - >/dev/null
# kubelet syncs projected secrets on its own cadence (up to ~a minute), then
# the agent's 5s poll notices and reloads.
new_serial="$old_serial"
for _ in $(seq 1 60); do
  new_serial=$(served_serial)
  [ "$new_serial" != "$old_serial" ] && [ "$new_serial" != "?" ] && break
  sleep 2
done
[ "$new_serial" != "$old_serial" ] || { echo "FAIL: served certificate never changed after rotation"; exit 1; }
[ "$(restarts_of tlse-0)" = "$before0" ] && [ "$(restarts_of tlse-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a cert rotation"; exit 1; }
[ "$(net_psql require 'SELECT 1')" = "1" ] || { echo "FAIL: tls broken after rotation"; exit 1; }
echo "OK new certificate served with no restarts"

echo "==> hostnossl reject turns plaintext away, tls stays"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/tlse" \
  -H 'content-type: application/json' \
  -d '{"pg_hba":[{"conn":"hostnossl","database":"all","user":"all","address":"0.0.0.0/0","method":"reject"}]}')
[ "$code" = "200" ] || { echo "FAIL: hostnossl patch returned $code"; exit 1; }
for _ in $(seq 1 30); do
  [ "$(net_psql disable 'SELECT 1')" = "DENIED" ] && break
  sleep 2
done
[ "$(net_psql disable 'SELECT 1')" = "DENIED" ] || { echo "FAIL: plaintext still allowed"; exit 1; }
[ "$(net_psql require 'SELECT 1')" = "1" ] || { echo "FAIL: tls clients broken by the reject rule"; exit 1; }
# Local socket access (the agent's own path) must survive any user rule.
[ "$(psql_on tlse-0 'SELECT 1')" = "1" ] || { echo "FAIL: local socket access broken"; exit 1; }
echo "OK plaintext rejected, tls and socket access intact"

echo "==> cleanup"
curl -s -X DELETE "$API/api/v1/clusters/tlse" >/dev/null
echo "tls e2e passed"
