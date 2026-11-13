#!/usr/bin/env bash
# Run in terminal B after start-server.sh. Checks the server is up and the
# token works. Calls no model and prints no secret.
set -euo pipefail

E2E_HOME="${E2E_HOME:-$HOME/.config/shepherd-e2e}"
PORT="${PORT:-8765}"
URL="http://127.0.0.1:$PORT/mcp"
BODY='{"jsonrpc":"2.0","id":1,"method":"ping"}'

# Without a token the server must answer 401.
code_noauth="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL" \
  -H 'Content-Type: application/json' -d "$BODY" || true)"
echo "no token   -> HTTP $code_noauth   (expect 401)"

# With the token it must answer 200. The token goes to curl on stdin, never on the command line.
code_auth="$(printf 'header = "Authorization: Bearer %s"\n' "$(cat "$E2E_HOME/token")" \
  | curl -s -o /dev/null -w '%{http_code}' -X POST "$URL" -K - \
      -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
      -d "$BODY" || true)"
echo "with token -> HTTP $code_auth   (expect 200)"

if [ "$code_noauth" = "401" ] && [ "$code_auth" = "200" ]; then
  echo "READY: go on with README section 4 (source ./mcp.sh)"
else
  echo "NOT READY: is terminal A running start-server.sh? (000 = nothing listening, 403 = Host header, 401 with token = wrong token)" >&2
  exit 1
fi
