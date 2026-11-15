#!/usr/bin/env bash
# The HOLDER_VERSION rule: a front that speaks another protocol number cannot attach, so
# MCP is down and the holder retries with growing delays, while the pane stays alive; a
# front that matches again adopts the pane with its state. No special build is needed:
# the holder starts with OPENAB_SHEPHERD_TEST_HOLDER_VERSION=@<file>, and every front
# reads the file when it starts.
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
holder="$(r_holder_pid)"
r_start t1 turns
r_check "the pane reaches done with 2 turns" r_wait_field t1 turn 2 20
pane_before="$(r_pane_pid)"

echo 999 > "$R_VERSION_FILE"
kill -USR2 "$holder"
end=$((SECONDS + 40))
while [ "$SECONDS" -lt "$end" ] && ! r_log_has 'respawning the front in 2000 ms'; do sleep 0.5; done
r_check "the front exits with status 78, twice" r_log_has 'exit status: 78' 2
r_check "the log names both numbers" r_log_has 'holder protocol [0-9]+, front 999'
r_check "the delays grow: 1000 ms then 2000 ms" r_log_has 'respawning the front in 2000 ms'
r_check "MCP is down" test "$(r_mcp_up && echo up || echo down)" = down
r_check "the holder is alive" r_quiet r_holder_pid
r_check "the pane's program is alive" r_quiet kill -0 "$pane_before"

: > "$R_VERSION_FILE"
r_check "a matching front attaches within 40 s" r_wait_mcp 40
r_check "the pane's program is the same process" test "$(r_pane_pid)" = "$pane_before"
r_check "turn is still 2" test "$(r_field t1 turn)" = 2
r_check "status is still done" test "$(r_field t1 status.state)" = "done"
r_check "the new front adopted the pane" r_log_has 'front: attached; adopted 1 pane\(s\), 0 replayed'
r_finish
