#!/usr/bin/env bash
# pglord-agent end-to-end: bootstrap a primary and a streaming standby in
# Docker, prove replication of real rows, then promote the standby.
set -euo pipefail

NET=pglord-e2e
PRIMARY=pg-e2e-primary
STANDBY=pg-e2e-standby
IMAGE=pglord-agent:dev
SU_PW=e2e-super-secret
REPL_PW=e2e-repl-secret

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
  -e PGLORD_AGENT_ROLE=primary \
  -e PGLORD_AGENT_SUPERUSER_PASSWORD="$SU_PW" \
  -e PGLORD_AGENT_REPL_PASSWORD="$REPL_PW" "$IMAGE" >/dev/null
wait_status 18008 "['ready']" "True" 60
wait_status 18008 "['in_recovery']" "False" 10
echo "OK primary ready"

echo "==> starting standby"
docker run -d --name "$STANDBY" --network "$NET" -p 18009:8008 \
  -e PGLORD_AGENT_ROLE=standby -e PGLORD_AGENT_PRIMARY_HOST="$PRIMARY" \
  -e PGLORD_AGENT_SUPERUSER_PASSWORD="$SU_PW" \
  -e PGLORD_AGENT_REPL_PASSWORD="$REPL_PW" "$IMAGE" >/dev/null
wait_status 18009 "['ready']" "True" 90
wait_status 18009 "['in_recovery']" "True" 10
echo "OK standby streaming (lsn: $(curl -s http://127.0.0.1:18009/status | python3 -c "import sys,json; print(json.load(sys.stdin)['lsn'])"))"

echo "==> checking /metrics"
curl -sf http://127.0.0.1:18008/metrics | grep -q '^pglord_pg_up 1' \
  || { echo "FAIL primary /metrics missing pglord_pg_up 1"; exit 1; }
curl -sf http://127.0.0.1:18008/metrics | grep -q '^pglord_instance_primary 1' \
  || { echo "FAIL primary /metrics missing pglord_instance_primary 1"; exit 1; }
curl -sf http://127.0.0.1:18009/metrics | grep -q '^pglord_instance_primary 0' \
  || { echo "FAIL standby /metrics missing pglord_instance_primary 0"; exit 1; }
curl -sf http://127.0.0.1:18008/metrics | grep -q '^pglord_pg_standby_replay_lag_bytes' \
  || { echo "FAIL primary /metrics missing standby lag series"; exit 1; }
echo "OK metrics exported"

echo "==> writing on primary"
docker exec -e PGPASSWORD="$SU_PW" "$PRIMARY" psql -U postgres -c "CREATE TABLE pglord_e2e(x int); INSERT INTO pglord_e2e VALUES (42);" >/dev/null

echo "==> verifying scram is enforced (wrong password must fail)"
if docker exec -e PGPASSWORD=wrong "$STANDBY" psql -h "$PRIMARY" -U postgres -c "SELECT 1" >/dev/null 2>&1; then
  echo "FAIL: wrong password was accepted"; exit 1
fi
echo "OK wrong password rejected"

echo "==> verifying replication on standby"
for _ in $(seq 1 30); do
  got=$(docker exec -e PGPASSWORD="$SU_PW" "$STANDBY" psql -U postgres -tAc "SELECT x FROM pglord_e2e" 2>/dev/null || echo "")
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
docker exec -e PGPASSWORD="$SU_PW" "$STANDBY" psql -U postgres -c "INSERT INTO pglord_e2e VALUES (43);" >/dev/null
echo "OK promoted node accepts writes"

echo "agent e2e passed"
