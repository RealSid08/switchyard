#!/usr/bin/env bash
# Starts a built switchyard binary with a fresh data directory, runs check-gateway.sh against
# it, verifies the single-process lock and token-path, then stops it.
# Usage: smoke-binary.sh PATH_TO_BINARY [EXPECTED_VERSION]
set -euo pipefail
bin=$1
expected=${2:-}
here=$(cd "$(dirname "$0")" && pwd)
port=$((17000 + RANDOM % 2000))
work=$(mktemp -d)
data="$work/data"
log="$work/server.log"
pid=""
stop() {
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null || true
    if [ "${RUNNER_OS:-}" = Windows ]; then taskkill //F //IM "$(basename "$bin")" > /dev/null 2>&1 || true; fi
    wait "$pid" 2>/dev/null || true
  fi
}
trap 'stop; cat "$log" 2>/dev/null || true; rm -rf "$work"' EXIT
fail() { echo "::error::$*"; exit 1; }

version=$("$bin" --version)
echo "$version"
if [ -n "$expected" ]; then
  [ "$version" = "switchyard $expected" ] || fail "binary reports '$version', expected 'switchyard $expected'"
fi
[ "$("$bin" --data-dir "$data" token-path | tr -d '\r')" = "$data/admin-token" ] || [ "${RUNNER_OS:-}" = Windows ] || fail "token-path output"
[ ! -e "$data" ] || fail "token-path created state"

"$bin" --data-dir "$data" --port "$port" > "$log" 2>&1 &
pid=$!
for _ in $(seq 1 60); do
  curl --silent --fail "http://127.0.0.1:$port/healthz" > /dev/null && break
  kill -0 "$pid" 2>/dev/null || fail "server exited during startup"
  sleep 0.5
done
token=$(tr -d '\r\n' < "$data/admin-token")
[[ "$token" == sy_admin_* ]] || fail "admin token not created"

bash "$here/check-gateway.sh" "http://127.0.0.1:$port" "$token" loopback

# A second process must refuse the locked data directory instead of serving it.
second=$((port + 1))
"$bin" --data-dir "$data" --port "$second" > "$work/second.log" 2>&1 &
other=$!
for _ in $(seq 1 20); do kill -0 "$other" 2>/dev/null || break; sleep 0.5; done
if kill -0 "$other" 2>/dev/null; then
  kill "$other" 2>/dev/null || true
  fail "second process kept running on a locked data directory"
fi
grep -q "Another Switchyard process" "$work/second.log" || fail "second process failed for the wrong reason: $(cat "$work/second.log")"
# token-path still works while the server holds the lock.
"$bin" --data-dir "$data" token-path > /dev/null || fail "token-path failed while the server runs"
echo "Binary smoke test passed"
