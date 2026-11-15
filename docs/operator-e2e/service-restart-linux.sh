#!/usr/bin/env bash
# Linux (p1): run the holder as a systemd user unit and check both kinds of restart.
#   systemctl --user reload    runs ExecReload (kill -USR2 $MAINPID): the pane survives.
#   systemctl --user restart   restarts the holder: the pane ends, as documented.
# It installs a throwaway unit (shepherd-e2e-restart.service) in ~/.config/systemd/user and
# removes it when it ends. It needs a systemd user session (over ssh: `loginctl
# enable-linger $USER` once, and XDG_RUNTIME_DIR set). Run it yourself; agents never start
# the server. Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

[ "$(uname -s)" = Linux ] || r_die "this script is for Linux; use service-restart-macos.sh on macOS"
systemctl --user show-environment >/dev/null 2>&1 \
  || r_die "no systemd user session (try: loginctl enable-linger $USER, then log in again; XDG_RUNTIME_DIR=/run/user/$(id -u))"
UNIT="shepherd-e2e-restart.service"
UNIT_FILE="$HOME/.config/systemd/user/$UNIT"

service_cleanup() {
  systemctl --user stop "$UNIT" 2>/dev/null
  rm -f "$UNIT_FILE"
  systemctl --user daemon-reload 2>/dev/null
  r_cleanup
}

main_pid() { systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null; }

r_setup
trap service_cleanup EXIT
: > "$R_LOG"
mkdir -p "$HOME/.config/systemd/user"
cat > "$UNIT_FILE" <<UNITEOF
[Unit]
Description=Shepherd restart-survival e2e (throwaway)

[Service]
Type=simple
WorkingDirectory=$R_WORK/server
Environment=OPENAB_SHEPHERD_TOKEN_FILE=$R_HOME/token
Environment=OPENAB_SHEPHERD_PROFILES=$R_HOME/profiles.json
Environment=PATH=/usr/local/bin:/usr/bin:/bin
ExecStart=$BIN hold -- --host 127.0.0.1 --port $PORT
ExecReload=/bin/kill -USR2 \$MAINPID
StandardOutput=append:$R_LOG
StandardError=append:$R_LOG
UNITEOF
r_port_in_use "$PORT" && r_die "port $PORT is in use; set PORT=<another port>"
systemctl --user daemon-reload
systemctl --user start "$UNIT" || r_die "systemctl start failed"
r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }

holder="$(main_pid)"
r_check "systemd runs the holder (pid ${holder:-none})" test -n "$holder" -a "${holder:-0}" != 0
r_start t1 turns
r_check "the pane reaches done with 2 turns" r_wait_field t1 turn 2 20
pane_before="$(r_pane_pid "$holder")"
front="$(r_front_pid "$holder")"

systemctl --user reload "$UNIT"
r_check "reload: a new front serves MCP" r_wait_new_front_of "$holder" "$front" 30
r_check "reload: the holder is the same process" test "$(main_pid)" = "$holder"
r_check "reload: the pane's program is the same process" test "$(r_pane_pid "$holder")" = "$pane_before"
r_check "reload: turn is still 2" test "$(r_field t1 turn)" = 2

systemctl --user restart "$UNIT"
r_wait_mcp 30
now="$(main_pid)"
r_check "restart: systemd runs a new holder (was $holder, now $now)" test "$now" != "$holder"
r_check "restart: the old pane's program ended" r_pane_gone "$pane_before"
r_check "restart: the pane is gone (not_found)" test "$(r_error get_status t1)" = not_found
r_check "restart: no pane process is left over" test "$(pgrep -f 'sleep 31337' | wc -l | tr -d ' ')" = 0
r_finish
