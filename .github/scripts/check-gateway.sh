#!/usr/bin/env bash
# Checks a running Switchyard: embedded UI and assets, health, auth boundaries and sessions.
# Usage: check-gateway.sh BASE_URL ADMIN_TOKEN MODE
#   MODE=loopback  the process binds loopback, so same-origin browser bootstrap must work
#   MODE=remote    the process binds a non-loopback address, so bootstrap must be refused
set -euo pipefail
base=$1
token=$2
mode=$3
fail() { echo "::error::$*"; exit 1; }
code() { curl --silent --output /dev/null --write-out '%{http_code}' "$@"; }
headers=$(mktemp)
body=$(mktemp)
trap 'rm -f "$headers" "$body"' EXIT

curl --fail --silent "$base/healthz" | grep -q '"status":"ok"' || fail "healthz"

# Embedded UI, every referenced asset, SPA fallback and security headers.
curl --fail --silent --dump-header "$headers" "$base/" > "$body"
grep -q 'id="root"' "$body" || fail "index.html is not the embedded UI"
grep -qi '^x-content-type-options: nosniff' "$headers" || fail "missing nosniff"
grep -qi "^content-security-policy: .*frame-ancestors 'none'" "$headers" || fail "missing CSP"
assets=$(grep -oE '(src|href)="/[^"]+"' "$body" | sed -E 's/^(src|href)="//; s/"$//' | sort -u)
[ -n "$assets" ] || fail "index.html references no assets"
for asset in $assets; do
  [ "$(code "$base$asset")" = 200 ] || fail "asset $asset not served"
done
curl --fail --silent "$base/connections" | grep -q 'id="root"' || fail "SPA fallback"
[ "$(code "$base/api/nope" -H "Authorization: Bearer $token")" = 404 ] || fail "unknown API path must be a JSON 404"

# Authentication boundaries.
[ "$(code "$base/api/connections")" = 401 ] || fail "admin API without credentials"
[ "$(code "$base/v1/models")" = 401 ] || fail "client API without a key"
[ "$(code "$base/v1/models" -H "Authorization: Bearer $token")" = 401 ] || fail "admin token accepted as a client key"
[ "$(code "$base/api/connections" -H "Authorization: Bearer $token")" = 200 ] || fail "admin token rejected"
[ "$(code "$base/api/connections" -H "Authorization: Bearer $token" -H 'Origin: https://evil.example')" = 403 ] || fail "cross-origin admin accepted"

# Usage and billing metadata belong to the admin API, including on fresh installs.
for endpoint in '/api/usage?window=24h' /api/usage/pricing /api/usage/sources /api/usage/monitors; do
  [ "$(code "$base$endpoint")" = 401 ] || fail "usage API exposed without credentials: $endpoint"
  [ "$(code "$base$endpoint" -H "Authorization: Bearer $token")" = 200 ] || fail "usage API unavailable: $endpoint"
  [ "$(code "$base$endpoint" -H "Authorization: Bearer $token" -H 'Origin: https://evil.example')" = 403 ] || fail "cross-origin usage API accepted: $endpoint"
done
[ "$(code "$base/api/usage?window=invalid" -H "Authorization: Bearer $token")" = 400 ] || fail "invalid usage window accepted"

# Token session, then logout revokes it.
curl --fail --silent --dump-header "$headers" -X POST "$base/api/session" -H "Authorization: Bearer $token" > /dev/null
cookie=$(grep -i '^set-cookie: sy_session=' "$headers" | head -n1 | sed -E 's/^[^:]+: *//; s/;.*$//' | tr -d '\r')
[ -n "$cookie" ] || fail "no session cookie"
grep -i '^set-cookie:' "$headers" | grep -qi 'httponly' || fail "session cookie is not HttpOnly"
[ "$(code "$base/api/overview" -H "Cookie: $cookie")" = 200 ] || fail "session cookie rejected"
[ "$(code -X DELETE "$base/api/session" -H "Cookie: $cookie")" = 200 ] || fail "logout failed"
[ "$(code "$base/api/overview" -H "Cookie: $cookie")" = 401 ] || fail "session survived logout"

# Browser bootstrap depends on the bind address.
host=${base#http://}
bootstrap=$(code -X POST "$base/api/session" -H 'Sec-Fetch-Site: same-origin' -H "Origin: http://$host")
if [ "$mode" = loopback ]; then
  [ "$bootstrap" = 200 ] || fail "loopback browser bootstrap returned $bootstrap"
else
  [ "$bootstrap" = 401 ] || fail "non-loopback bind allowed tokenless bootstrap ($bootstrap)"
fi
echo "Gateway checks passed ($mode)"
