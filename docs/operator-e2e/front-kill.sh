#!/usr/bin/env bash
# Kill -9 the front (the MCP server) of a running holder and check that the pane, its
# turn and its status come back, and that the holder respawns the front by itself.
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
holder="$(r_holder_pid)"
r_start t1 turns
r_check "the pane reaches done with 2 turns" r_wait_field t1 turn 2 20
r_check "the pane's status is done" r_wait_field t1 status.state "done" 20
pane_before="$(r_pane_pid)"
front_before="$(r_front_pid)"
r_say "holder $holder, front $front_before, pane leader $pane_before"

kill -9 "$front_before"
start=$SECONDS
r_check "the holder starts another front" r_wait_new_front "$front_before" 30
r_say "MCP was back $((SECONDS - start)) s after the kill"

r_check "the holder is the same process" test "$(r_holder_pid)" = "$holder"
r_check "the pane's program is the same process" test "$(r_pane_pid)" = "$pane_before"
r_check "list_panes still shows the pane" r_wait_field t1 live true 10
r_check "turn is still 2" test "$(r_field t1 turn)" = 2
r_check "status is still done" test "$(r_field t1 status.state)" = "done"
r_check "the holder logged the exit of the front" r_log_has 'holder: front 1 exited'
r_check "the holder logged the respawn" r_log_has 'respawning the front in 1000 ms'
r_check "the new front adopted one pane, none replayed" r_log_has 'front: attached; adopted 1 pane\(s\), 0 replayed'

# A second kill right away: the delay doubles; the pane is untouched.
front_two="$(r_front_pid)"
kill -9 "$front_two"
r_check "a second kill is survived too" r_wait_new_front "$front_two" 30
r_check "the pane's program is still the same process" test "$(r_pane_pid)" = "$pane_before"
r_check "turn is still 2 after two kills" test "$(r_field t1 turn)" = 2

# The pane still works through the new front.
r_check "send_input reaches the adopted pane" r_quiet send_input t1 "hello after the restart"
r_finish
