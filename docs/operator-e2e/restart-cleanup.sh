#!/usr/bin/env bash
# Stop the holder that holder-start.sh (or a scenario run with KEEP=1) left behind, which
# ends its panes, and remove the throwaway token, profiles and working directories.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

unset KEEP
r_stop_holder
case "$R_WORK" in */shepherd-e2e-restart*) rm -rf "$R_WORK" ;; *) r_die "refusing to remove $R_WORK" ;; esac
case "$R_HOME" in */shepherd-e2e-restart*) rm -rf "$R_HOME" ;; *) r_die "refusing to remove $R_HOME" ;; esac
echo "removed $R_WORK and $R_HOME"
if r_port_in_use "$PORT"; then echo "WARNING: something still listens on port $PORT" >&2; fi
if pgrep -f 'sleep 31337' >/dev/null 2>&1; then echo "WARNING: a pane's sleep 31337 is still running" >&2; fi
exit 0
