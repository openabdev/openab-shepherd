#!/usr/bin/env bash
# Step 1 of the operator E2E runbook (README.md): start the Shepherd server in the FOREGROUND.
# Run this in its own terminal ("terminal A") and leave it running.
# It only starts the server. It calls no model and prints no secret.
#
#   bash start-server.sh            # default: 127.0.0.1:8765
#   PORT=8766 bash start-server.sh  # another port if 8765 is taken
#
# Stop it with Ctrl-C (the server ends its panes on SIGINT).
set -euo pipefail

E2E_HOME="${E2E_HOME:-$HOME/.config/shepherd-e2e}"
WORK="${E2E_WORK:-/tmp/shepherd-e2e}"
BIN="${SHEPHERD_BIN:-$HOME/.local/bin/openab-shepherd}"
HOST="127.0.0.1"
PORT="${PORT:-8765}"

die() { echo "start-server: $*" >&2; exit 1; }

# 1. The binary.
[ -x "$BIN" ] || die "no executable at $BIN (build it on a separate build host first, see README section 0)"

# 2. The token and profiles that setup.sh creates.
[ -f "$E2E_HOME/token" ]         || die "no token at $E2E_HOME/token (run: bash setup.sh)"
[ -f "$E2E_HOME/profiles.json" ] || die "no profiles at $E2E_HOME/profiles.json (run: bash setup.sh)"
[ -d "$WORK/server" ]            || die "no $WORK/server (run: bash setup.sh)"
tmode="$(stat -f '%Lp' "$E2E_HOME/token")"
[ "$tmode" = "600" ] || die "token mode is $tmode, expected 600 (chmod 600 $E2E_HOME/token)"
pmode="$(stat -f '%Lp' "$E2E_HOME/profiles.json")"
case "$pmode" in 600|644) ;; *) die "profiles mode is $pmode, expected 600 or 644" ;; esac

# 3. The coding CLIs must be reachable, because panes inherit the server's PATH.
#    Add the usual install directories if they are not on PATH yet.
export PATH="$HOME/.local/bin:$HOME/.bun/bin:$PATH"
command -v devin    >/dev/null || die "devin is not on PATH"
command -v opencode >/dev/null || die "opencode is not on PATH"

# 4. The backend must be native (the default). A leftover variable would change that.
if [ -n "${OPENAB_SHEPHERD_BACKEND:-}" ] && [ "${OPENAB_SHEPHERD_BACKEND}" != "native" ]; then
  die "OPENAB_SHEPHERD_BACKEND is set to '${OPENAB_SHEPHERD_BACKEND}'; unset it (hc was removed)"
fi
unset OPENAB_SHEPHERD_BACKEND

# 5. The port must be free.
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  die "port $PORT is already in use; stop the other process or set PORT=<another port>"
fi

echo "start-server: binary   $BIN"
echo "start-server: devin    $(command -v devin)"
echo "start-server: opencode $(command -v opencode)"
echo "start-server: listening address $HOST:$PORT (loopback only)"
echo "start-server: expect the line:  openab-shepherd: listening on http://$HOST:$PORT/mcp (backend: native)"
echo "start-server: leave this terminal open; press Ctrl-C to stop the server and end its panes."
echo

# The server must start from a folder outside every git repository and not the
# token's folder; $WORK/server is that folder.
cd "$WORK/server"
exec env \
  OPENAB_SHEPHERD_TOKEN_FILE="$E2E_HOME/token" \
  OPENAB_SHEPHERD_PROFILES="$E2E_HOME/profiles.json" \
  "$BIN" --host "$HOST" --port "$PORT"
