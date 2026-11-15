#!/usr/bin/env bash
# Start a real `openab-shepherd hold` on a throwaway token, profiles file and port, and
# leave it running for manual poking. Run it yourself; agents never start the server.
#
#   bash holder-start.sh            # port 8766 (PORT=<n> to change)
#   bash restart-cleanup.sh         # stops the holder (which ends the panes) and removes the files
#
# It prints the holder and front pids. Try by hand: `source mcp.sh` with the printed
# SHEPHERD_URL and SHEPHERD_TOKEN_FILE, then `start_pane turns "$R_WORK/turns" t1`.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_setup
r_start_holder
disown 2>/dev/null || true
echo "SHEPHERD_URL=$SHEPHERD_URL"
echo "SHEPHERD_TOKEN_FILE=$SHEPHERD_TOKEN_FILE"
echo "R_WORK=$R_WORK"
echo "holder pid: $(r_holder_pid)"
echo "front pid:  $(r_front_pid)"
echo "log:        $R_LOG"
echo "stop with:  bash $HERE/restart-cleanup.sh"
