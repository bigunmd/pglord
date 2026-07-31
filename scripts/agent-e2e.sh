#!/usr/bin/env bash
# pglord-agent end-to-end: bootstrap a primary and a streaming standby in
# Docker, prove replication of real rows, then promote the standby.
set -euo pipefail

NET=pglord-e2e
PRIMARY=pg-e2e-primary
STANDBY=pg-e2e-standby
IMAGE=pglord-agent:dev

cleanup() {
  docker rm -f "$PRIMARY" "$STANDBY" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

wait_status() { # host_port jq_expr expected timeout_s
  local port=$1 expr=$2 expected=$3 timeout=${4:-60}
  for _ in $(seq 1 "$timeout"); do
    got=$(curl -sf "http://127.0.0.1:$port/status" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)$expr)" 2>/dev/null || echo "")
    [ "$got" = "$expected" ] && return 0
    sleep 1
  done
  echo "TIMEOUT waiting for :$port/status $expr == $expected (last: '$got')"
  docker logs --tail 20 "$PRIMARY" || true
  docker logs --tail 20 "$STANDBY" || true
  return 1
}

echo "==> building agent image"
docker build -q -f deploy/Dockerfile.agent -t "$IMAGE" . >/dev/null

echo "==> starting primary"
docker network create "$NET" >/dev/null
docker run -d --name "$PRIMARY" --network "$NET" -p 18008:8008 \
  -e PGLORD_AGENT_ROLE=primary "$IMAGE" >/dev/null
wait_status 18008 "['ready']" "True" 60
wait_status 18008 "['in_recovery']" "False" 10
echo "OK primary ready"

echo "==> starting standby"
docker run -d --name "$STANDBY" --network "$NET" -p 18009:8008 \
  -e PGLORD_AGENT_ROLE=standby -e PGLORD_AGENT_PRIMARY_HOST="$PRIMARY" "$IMAGE" >/dev/null
wait_status 18009 "['ready']" "True" 90
wait_status 18009 "['in_recovery']" "True" 10
echo "OK standby streaming (lsn: $(curl -s http://127.0.0.1:18009/status | python3 -c "import sys,json; print(json.load(sys.stdin)['lsn'])"))"

echo "==> writing on primary"
docker exec "$PRIMARY" psql -U postgres -c "CREATE TABLE pglord_e2e(x int); INSERT INTO pglord_e2e VALUES (42);" >/dev/null

echo "==> verifying replication on standby"
for _ in $(seq 1 30); do
  got=$(docker exec "$STANDBY" psql -U postgres -tAc "SELECT x FROM pglord_e2e" 2>/dev/null || echo "")
  [ "$got" = "42" ] && break
  sleep 1
done
[ "$got" = "42" ] || { echo "FAIL: row did not replicate (got '$got')"; exit 1; }
echo "OK row replicated"

echo "==> promoting standby"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18009/promote)
[ "$code" = "202" ] || { echo "FAIL: promote returned $code"; exit 1; }
wait_status 18009 "['in_recovery']" "False" 30
echo "OK standby promoted"

echo "==> verifying writes on promoted node"
docker exec "$STANDBY" psql -U postgres -c "INSERT INTO pglord_e2e VALUES (43);" >/dev/null
echo "OK promoted node accepts writes"

echo "agent e2e passed"
