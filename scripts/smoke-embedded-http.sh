#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/release/solodock}
[[ -x $binary ]] || { printf '%s\n' 'embedded binary is missing' >&2; exit 2; }
fixture=$(mktemp -d)
pid=''
cleanup() {
  if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid"
    wait "$pid" || true
  fi
  rm -rf -- "$fixture"
}
trap cleanup EXIT
trap 'status=$?; printf "embedded HTTP smoke failed at line %s (status %s)\n" "$LINENO" "$status" >&2; [[ -r $fixture/stderr ]] && sed -n "1,120p" "$fixture/stderr" >&2; exit "$status"' ERR
chmod 0700 "$fixture"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
management_host='solodock.example.invalid'
origin="https://$management_host"
cat >"$fixture/config.toml" <<EOF
schema_version = 1
listen_address = "127.0.0.1:$port"
public_origin = "$origin"
state_directory = "$fixture/state"
runtime_directory = "$fixture/run"
allowed_bind_roots = []
EOF
chmod 0600 "$fixture/config.toml"
SOLODOCK_CONFIG_PATH="$fixture/config.toml" "$binary" >"$fixture/stdout" 2>"$fixture/stderr" &
pid=$!
for _ in $(seq 1 100); do
  if curl --fail --silent "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then break; fi
  kill -0 "$pid" 2>/dev/null || { printf '%s\n' 'embedded binary exited early' >&2; exit 1; }
  sleep 0.1
done

curl --fail --silent -H "host: $management_host" \
  "http://127.0.0.1:$port/" >"$fixture/index"
grep -q '<div id="app"></div>' "$fixture/index"
grep -q 'href="/favicon.svg"' "$fixture/index"
curl --fail --silent -D "$fixture/favicon.headers" \
  "http://127.0.0.1:$port/favicon.svg" >"$fixture/favicon.svg"
grep -qi '^content-type: image/svg+xml' "$fixture/favicon.headers"
grep -q '<svg' "$fixture/favicon.svg"
asset=$(grep -oE '/assets/[^" ]+\.js' "$fixture/index" | head -n 1)
[[ -n $asset ]]
curl --fail --silent -H "host: $management_host" \
  "http://127.0.0.1:$port$asset" >/dev/null
curl --fail --silent -H "host: $management_host" -H 'accept: text/html' \
  "http://127.0.0.1:$port/apps/fixture-spa-route" | cmp -s - "$fixture/index"

bootstrap=$(tr -d '\n' <"$fixture/run/bootstrap.token")
curl --fail --silent -X POST "http://127.0.0.1:$port/api/v1/auth/bootstrap" \
  -H "host: $management_host" -H 'content-type: application/json' -H "origin: $origin" \
  --data "{\"bootstrap_token\":\"$bootstrap\",\"password\":\"correct horse battery\"}" >/dev/null
curl --fail --silent -D "$fixture/login.headers" -o /dev/null \
  -X POST "http://127.0.0.1:$port/api/v1/auth/login" \
  -H "host: $management_host" -H 'content-type: application/json' -H "origin: $origin" \
  --data '{"username":"admin","password":"correct horse battery"}'
session=$(sed -n 's/^set-cookie: \(__Host-solodock_session=[^;]*\).*/\1/ip' "$fixture/login.headers")
csrf_cookie=$(sed -n 's/^set-cookie: \(__Host-solodock_csrf=[^;]*\).*/\1/ip' "$fixture/login.headers")
[[ -n $session && -n $csrf_cookie ]]
curl --fail --silent "http://127.0.0.1:$port/api/v1/me" \
  -H "host: $management_host" -H "cookie: $session; $csrf_cookie" | grep -q '"username":"admin"'

kill -TERM "$pid"
wait "$pid"
pid=''
